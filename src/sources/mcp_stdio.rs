use super::SourceRequest;
use super::process::{ProcessSpec, RunningProcess};
use crate::core::{Tool, now_ms};
use crate::crypto::random_token;
use crate::rpc::UpstreamError;
use crate::rpc::{self, decode_batch, decode_tools_list, empty, event_stream, id_key, parse_error, rpc_error};
use crate::store::{StdioSession, Store};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::join_all;
use http::{HeaderValue, Method, StatusCode};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::wrappers::BroadcastStream;

const REQUEST_TIMEOUT: Duration = Duration::from_mins(10);

const START_TIMEOUT: Duration = Duration::from_mins(1);

const IDLE_TIMEOUT_MS: i64 = 30 * 60_000;
const MAX_SESSIONS: usize = 8;

pub struct StdioSpec {
    pub name: String,
    pub command: Vec<String>,
    pub env: IndexMap<String, String>,
    pub cwd: Option<String>,
    pub log_file: PathBuf,
}

type Message = Map<String, Value>;

struct Session {
    id: String,
    process: RunningProcess,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    events: broadcast::Sender<Bytes>,
    last_used: AtomicI64,
}

impl Session {
    fn touch(&self) {
        self.last_used.store(now_ms(), Ordering::Relaxed);
    }

    fn last_used(&self) -> i64 {
        self.last_used.load(Ordering::Relaxed)
    }

    fn deliver(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Message>(line) else {
            return;
        };
        let is_request = message.get("method").is_some_and(Value::is_string);
        let waiting = if is_request {
            None
        } else {
            self.pending.lock().ok().and_then(|mut pending| pending.remove(&id_key(message.get("id"))))
        };

        match waiting {
            Some(reply) => {
                let _ = reply.send(Value::Object(message));
            }
            None => {
                let _ = self.events.send(Bytes::from(format!("event: message\ndata: {line}\n\n")));
            }
        }
    }

    fn fail_pending(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }
}

fn porchlight_initialize() -> Value {
    json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "porchlight", "version": crate::core::VERSION } })
}

pub struct StdioSource {
    spec: StdioSpec,
    store: Store,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sessions_live: AtomicUsize,
    resuming: tokio::sync::Mutex<()>,
    me: Weak<StdioSource>,
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StdioSource {
    pub fn start(spec: StdioSpec, store: Store) -> Arc<Self> {
        let source = Arc::new_cyclic(|me| Self {
            spec,
            store,
            sessions: Mutex::new(HashMap::new()),
            sessions_live: AtomicUsize::new(0),
            resuming: tokio::sync::Mutex::new(()),
            me: me.clone(),
            sweeper: Mutex::new(None),
        });
        let weak = Arc::downgrade(&source);
        let sweeper = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_mins(1));
            interval.tick().await;

            loop {
                interval.tick().await;
                let Some(source) = weak.upgrade() else { return };
                source.sweep().await;
            }
        });

        if let Ok(mut slot) = source.sweeper.lock() {
            *slot = Some(sweeper);
        }

        source
    }

    pub fn describe(&self) -> String {
        self.spec.command.join(" ")
    }

    fn live(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().ok().and_then(|sessions| sessions.get(id).cloned())
    }

    fn all(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().map(|sessions| sessions.values().cloned().collect()).unwrap_or_default()
    }

    fn remember(&self, session: &Session) {
        let _ = self.store.stdio_sessions_touch(&[(session.id.clone(), session.last_used())]);
    }

    async fn close(&self, session: &Arc<Session>) {
        let removed = self.sessions.lock().ok().and_then(|mut sessions| sessions.remove(&session.id));
        session.fail_pending();
        session.process.terminate().await;

        if removed.is_some() {
            self.sessions_live.fetch_sub(1, Ordering::AcqRel);
            self.remember(session);
        }
    }

    fn reserve(&self) -> bool {
        self.sessions_live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < MAX_SESSIONS).then_some(count + 1))
            .is_ok()
    }

    async fn evict_idlest(&self) -> bool {
        let idlest = self
            .all()
            .into_iter()
            .filter(|session| session.pending.lock().is_ok_and(|pending| pending.is_empty()))
            .min_by_key(|session| session.last_used());

        match idlest {
            Some(session) => {
                self.close(&session).await;
                true
            }
            None => false,
        }
    }

    async fn open(&self, id: Option<String>) -> Result<Arc<Session>, UpstreamError> {
        while !self.reserve() {
            if !self.evict_idlest().await {
                return Err(UpstreamError::new(format!("{} has reached its session limit", self.spec.name)));
            }
        }

        let session = match self.spawn(id) {
            Ok(session) => session,
            Err(error) => {
                self.sessions_live.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };

        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(session.id.clone(), session.clone());
        }

        Ok(session)
    }

    fn spawn(&self, id: Option<String>) -> Result<Arc<Session>, UpstreamError> {
        let id = id.unwrap_or_else(|| random_token(16));
        let (process, mut lines) = RunningProcess::start(&ProcessSpec {
            command: self.spec.command.clone(),
            cwd: self.spec.cwd.clone(),
            env: self.spec.env.clone(),
            log_file: self.spec.log_file.clone(),
        })?;
        let (events, _) = broadcast::channel(256);
        let session = Arc::new(Session {
            id,
            process,
            pending: Mutex::new(HashMap::new()),
            events,
            last_used: AtomicI64::new(now_ms()),
        });

        let reader = Arc::downgrade(&session);
        let owner = self.me.clone();
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                match reader.upgrade() {
                    Some(session) => session.deliver(&line),
                    None => return,
                }
            }

            if let (Some(source), Some(session)) = (owner.upgrade(), reader.upgrade()) {
                source.close(&session).await;
            }
        });

        Ok(session)
    }

    async fn one_shot(&self) -> Result<Arc<Session>, UpstreamError> {
        let session = self.spawn(None)?;
        if let Err(error) = self.initialize(&session, porchlight_initialize()).await {
            self.close(&session).await;
            return Err(error);
        }

        Ok(session)
    }

    async fn send(
        &self,
        session: &Session,
        message: &Message,
        timeout: Duration,
    ) -> Result<Option<Value>, UpstreamError> {
        session.touch();
        if message.get("method").and_then(Value::as_str) == Some("server/discover") {
            return Ok(Some(rpc::rpc_result(
                message.get("id"),
                json!({
                    "protocolVersions": ["2025-06-18", "2025-11-25", "2026-07-28"],
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": self.spec.name, "version": crate::core::VERSION }
                }),
            )));
        }
        let text = Value::Object(message.clone()).to_string();
        let is_request = message.get("method").is_some_and(Value::is_string) && message.contains_key("id");

        if !is_request {
            return match tokio::time::timeout(timeout, session.process.write(&text)).await {
                Ok(result) => result.map(|()| None),
                Err(_) => Ok(Some(rpc_error(message.get("id"), -32001, "Request timed out"))),
            };
        }

        let id = message.get("id");
        let key = id_key(id);
        let (sender, receiver) = oneshot::channel();
        let added = session.pending.lock().is_ok_and(|mut pending| {
            if pending.contains_key(&key) {
                return false;
            }

            pending.insert(key.clone(), sender);
            true
        });

        if !added {
            return Ok(Some(rpc_error(id, -32600, "A request with this id is already in progress")));
        }

        let operation = async {
            session.process.write(&text).await?;
            Ok::<Option<Value>, UpstreamError>(receiver.await.ok())
        };
        let reply = match tokio::time::timeout(timeout, operation).await {
            Ok(Ok(Some(value))) => value,
            Ok(Ok(None)) => rpc_error(id, -32000, &format!("{} stopped", self.spec.name)),
            Ok(Err(error)) => {
                if let Ok(mut pending) = session.pending.lock() {
                    pending.remove(&key);
                }
                return Err(error);
            }
            Err(_) => rpc_error(id, -32001, "Request timed out"),
        };

        if let Ok(mut pending) = session.pending.lock() {
            pending.remove(&key);
        }

        Ok(Some(reply))
    }

    async fn initialize(&self, session: &Session, params: Value) -> Result<(), UpstreamError> {
        let message =
            json!({ "jsonrpc": "2.0", "id": "porchlight-initialize", "method": "initialize", "params": params });
        let reply = self.send(session, message.as_object().unwrap_or(&Map::new()), START_TIMEOUT).await?;

        if reply.as_ref().and_then(|reply| reply.get("result")).is_none() {
            return Err(UpstreamError::new(format!(
                "{} didn't start. See {}",
                self.spec.name,
                self.spec.log_file.display()
            )));
        }

        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.send(session, initialized.as_object().unwrap_or(&Map::new()), START_TIMEOUT).await.map(drop)
    }

    async fn resume(&self, id: &str) -> Result<Option<Arc<Session>>, UpstreamError> {
        let saved = self.store.stdio_session_get(id, &self.spec.name).ok().flatten();
        let Some(saved) = saved.filter(|saved| now_ms() - saved.last_used <= IDLE_TIMEOUT_MS) else {
            return Ok(None);
        };
        let session = self.open(Some(id.to_owned())).await?;
        let params = serde_json::from_str(&saved.initialize).unwrap_or_else(|_| porchlight_initialize());

        if let Err(error) = self.initialize(&session, params).await {
            self.close(&session).await;
            return Err(error);
        }

        let short: String = id.chars().take(8).collect();
        let _ = self.store.audit_write("session.resumed", &format!("{} | {short}", self.spec.name));

        Ok(Some(session))
    }

    async fn find(&self, id: &str) -> Result<Option<Arc<Session>>, UpstreamError> {
        if let Some(session) = self.live(id) {
            return Ok(Some(session));
        }

        let _resuming = self.resuming.lock().await;

        if let Some(session) = self.live(id) {
            return Ok(Some(session));
        }

        self.resume(id).await
    }

    async fn post(&self, request: &SourceRequest) -> Result<Response, UpstreamError> {
        let Some(decoded) = decode_batch::<Message>(&request.body) else {
            return Ok(parse_error());
        };
        let initialize = decoded.items.iter().find(|message| message.get("method") == Some(&json!("initialize")));
        let session_id = request.header("mcp-session-id");
        if let (Some(message), Some(id)) = (initialize, session_id)
            && self.find(id).await?.is_some()
        {
            return Ok(rpc::json(
                &rpc_error(message.get("id"), -32600, "initialize cannot use an existing session"),
                StatusCode::OK,
            ));
        }
        let stateless = initialize.is_none() && session_id.is_none();
        let session = match (initialize, session_id) {
            (Some(_), _) => self.open(None).await.map(Some),
            (None, Some(id)) => self.find(id).await,
            (None, None) => self.one_shot().await.map(Some),
        };
        let session = match session {
            Ok(Some(session)) => session,
            Ok(None) => return Ok(rpc::json(&rpc_error(None, -32001, "Session not found"), StatusCode::NOT_FOUND)),
            Err(error) => {
                let id = initialize
                    .and_then(|message| message.get("id"))
                    .or_else(|| decoded.items.first().and_then(|message| message.get("id")));
                return Ok(rpc::json(&rpc_error(id, -32000, &error.message), StatusCode::OK));
            }
        };

        if let Some(message) = initialize {
            let params = message.get("params").cloned().unwrap_or_else(porchlight_initialize);
            let _ = self.store.stdio_session_save(&StdioSession {
                id: session.id.clone(),
                server: self.spec.name.clone(),
                initialize: params.to_string(),
                last_used: session.last_used(),
            });
        }

        let replies = join_all(decoded.items.iter().map(|message| self.send(&session, message, REQUEST_TIMEOUT))).await;
        if stateless {
            self.close(&session).await;
        }
        let mut answered = Vec::new();
        for reply in replies {
            match reply {
                Ok(Some(value)) => {
                    if initialize.is_some() && value.get("result").is_none() {
                        self.close(&session).await;
                    }
                    answered.push(value);
                }
                Ok(None) => {}
                Err(error) => {
                    if initialize.is_some() {
                        self.close(&session).await;
                    }
                    return Err(error);
                }
            }
        }
        let body = match (decoded.batch, answered) {
            (_, answered) if answered.is_empty() => None,
            (true, answered) => Some(Value::Array(answered)),
            (false, answered) => answered.into_iter().next(),
        };
        let mut response = match body {
            None => empty(StatusCode::ACCEPTED),
            Some(body) => rpc::json(&body, StatusCode::OK),
        };

        if !stateless && let Ok(value) = HeaderValue::from_str(&session.id) {
            response.headers_mut().insert("mcp-session-id", value);
        }

        Ok(response)
    }

    pub async fn handle(&self, request: SourceRequest) -> Result<Response, UpstreamError> {
        if request.method == Method::POST {
            return self.post(&request).await;
        }

        let Some(session_id) = request.header("mcp-session-id").map(str::to_owned) else {
            return Ok(empty(StatusCode::NOT_FOUND));
        };

        if request.method == Method::GET {
            return Ok(match self.find(&session_id).await? {
                Some(session) => {
                    let events = BroadcastStream::new(session.events.subscribe()).filter_map(|event| {
                        futures::future::ready(event.ok().map(Ok::<Bytes, std::convert::Infallible>))
                    });
                    event_stream(Body::from_stream(events))
                }
                None => empty(StatusCode::NOT_FOUND),
            });
        }

        let live = self.live(&session_id);
        let saved = self.store.stdio_session_get(&session_id, &self.spec.name).ok().flatten();
        let _ = self.store.stdio_session_remove(&session_id);

        if let Some(session) = &live {
            self.close(session).await;
        }

        Ok(empty(if live.is_some() || saved.is_some() { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }))
    }

    async fn handshake(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, UpstreamError> {
        let session = self.spawn(None)?;
        let message = json!({ "jsonrpc": "2.0", "id": 2, "method": method, "params": params });
        let reply = match self.initialize(&session, porchlight_initialize()).await {
            Ok(()) => self.send(&session, message.as_object().unwrap_or(&Map::new()), timeout).await,
            Err(error) => Err(error),
        };
        self.close(&session).await;

        Ok(reply?.unwrap_or(Value::Null))
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        let reply = self.handshake("tools/list", json!({}), START_TIMEOUT).await?;

        decode_tools_list(&reply.to_string(), &self.spec.name)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, UpstreamError> {
        self.handshake("tools/call", json!({ "name": name, "arguments": arguments }), Duration::from_mins(1)).await
    }

    async fn sweep(&self) {
        let now = now_ms();
        let live = self.all();

        for session in live.iter().filter(|session| now - session.last_used() > IDLE_TIMEOUT_MS) {
            self.close(session).await;
        }

        let touched: Vec<(String, i64)> =
            live.iter().map(|session| (session.id.clone(), session.last_used())).collect();
        let _ = self.store.stdio_sessions_touch(&touched);
        let _ = self.store.stdio_sessions_prune(&self.spec.name, now - IDLE_TIMEOUT_MS);
    }

    pub async fn shutdown(&self) {
        if let Some(sweeper) = self.sweeper.lock().ok().and_then(|mut slot| slot.take()) {
            sweeper.abort();
        }

        for session in self.all() {
            self.close(&session).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_blocked_stdin_write_is_bounded_by_the_request_timeout() {
        let log_file = std::env::temp_dir().join(format!("porchlight-stdin-timeout-{}", std::process::id()));
        let source = StdioSource::start(
            StdioSpec {
                name: "blocked".to_owned(),
                command: vec!["/bin/sleep".to_owned(), "60".to_owned()],
                env: IndexMap::new(),
                cwd: None,
                log_file,
            },
            Store::in_memory().unwrap(),
        );
        let session = source.open(None).await.unwrap();
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": { "large": "x".repeat(1024 * 1024) }
        })
        .as_object()
        .cloned()
        .unwrap();
        let reply = source.send(&session, &message, Duration::from_millis(50)).await.unwrap().unwrap();

        assert_eq!(reply["error"]["message"], "Request timed out");
        source.close(&session).await;
        source.shutdown().await;
    }

    #[tokio::test]
    async fn failed_initialize_closes_the_new_session() {
        let log_file = std::env::temp_dir().join(format!("porchlight-failed-init-{}", std::process::id()));
        let source = StdioSource::start(
            StdioSpec {
                name: "failed-init".to_owned(),
                command: vec!["/usr/bin/false".to_owned()],
                env: IndexMap::new(),
                cwd: None,
                log_file,
            },
            Store::in_memory().unwrap(),
        );
        let request = SourceRequest {
            method: Method::POST,
            headers: http::HeaderMap::new(),
            body: Bytes::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {}
                })
                .to_string(),
            ),
        };

        let _ = source.post(&request).await;

        assert_eq!(source.sessions_live.load(Ordering::Acquire), 0);
        source.shutdown().await;
    }
}
