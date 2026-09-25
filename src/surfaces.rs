use crate::core::{now_ms, truncate};
use crate::crypto::{hmac, safe_equal};
use crate::links::{LinkEntry, ServerState, ToolOrigin};
use crate::rpc::{
    self, Keep, ListKey, call_outcomes, decode_batch, filter_list, id_key, parse_error, payloads_within, rpc_error,
};
use crate::sources::{SourceRequest, UpstreamError};
use crate::store::{AuditFields, Outcome, Store, StoreError};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderValue, Method, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct Exchange {
    pub request: SourceRequest,
    pub server: LinkEntry,
    pub grant: String,
    pub caller: String,
    pub source: String,
}

#[derive(Clone, Deserialize)]
struct RpcMessage {
    #[serde(default, deserialize_with = "rpc::present")]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

const AUDIT_COPY_LIMIT: usize = 8 * 1024 * 1024;

const HANDSHAKE_METHODS: [&str; 3] = ["initialize", "server/discover", "ping"];

const CLIENT_METHODS: [&str; 14] = [
    "initialize",
    "server/discover",
    "ping",
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/templates/list",
    "resources/read",
    "resources/subscribe",
    "resources/unsubscribe",
    "prompts/list",
    "prompts/get",
    "completion/complete",
    "logging/setLevel",
];

fn param<'a>(message: &'a RpcMessage, key: &str) -> &'a str {
    message.params.as_ref().and_then(|params| params.get(key)).and_then(Value::as_str).unwrap_or_default()
}

fn uri_of(message: &RpcMessage) -> &str {
    param(message, "uri")
}

fn tool_name(message: &RpcMessage) -> &str {
    param(message, "name")
}

fn own_resource(uri: &str) -> bool {
    uri.starts_with("porchlight://")
}

fn refusal(message: &RpcMessage, server: &LinkEntry) -> Option<String> {
    let method = message.method.as_deref()?;

    if method.starts_with("notifications/") {
        return None;
    }

    if !CLIENT_METHODS.contains(&method) {
        return Some(format!("Method not allowed: {method}"));
    }

    let checked_each = method.starts_with("tools/")
        || (server.filter_resources && method == "resources/list")
        || (method == "resources/read" && own_resource(uri_of(message)));

    (!server.default_on && !checked_each && !HANDSHAKE_METHODS.contains(&method))
        .then(|| format!("Only the allowed tools are shared, not {method}"))
}

fn input_of(message: &RpcMessage) -> String {
    let arguments = message.params.as_ref().and_then(|params| params.get("arguments")).cloned().unwrap_or(json!({}));

    truncate(&arguments.to_string(), 500)
}

fn readable(server: &LinkEntry, uri: &str) -> bool {
    if !own_resource(uri) {
        return server.default_on;
    }

    server.resources.get(uri).is_some_and(|name| server.allowed.contains(name))
}

fn origin(server: &LinkEntry, name: &str) -> ToolOrigin {
    server.origins.get(name).cloned().unwrap_or_else(|| ToolOrigin { app: server.name.clone(), tool: name.to_owned() })
}

fn subject_of(server: &LinkEntry, message: &RpcMessage) -> String {
    if message.method.as_deref() != Some("resources/read") {
        return tool_name(message).to_owned();
    }

    let uri = uri_of(message);
    server.resources.get(uri).cloned().unwrap_or_else(|| uri.to_owned())
}

struct Context {
    store: Store,
    server: LinkEntry,
    caller: String,
    source: String,
}

impl Context {
    fn refuse(&self, event: &str, subject: &str, input: Option<String>) -> Result<(), StoreError> {
        let blocked_tool = event == "tool.blocked";
        let at = origin(&self.server, subject);

        self.store.audit_record(
            event,
            AuditFields {
                client: Some(self.caller.clone()),
                app: Some(if blocked_tool { at.app } else { self.server.name.clone() }),
                subject: Some(if blocked_tool { at.tool } else { subject.to_owned() }),
                source: Some(self.source.clone()),
                outcome: Some(Outcome::Refused),
                input,
                ..AuditFields::default()
            },
        )
    }

    fn record_call(&self, call: &RpcMessage, outcome: Outcome, duration_ms: i64, detail: &str) {
        let at = origin(&self.server, &subject_of(&self.server, call));
        let recorded = self.store.audit_record(
            "tool.called",
            AuditFields {
                client: Some(self.caller.clone()),
                app: Some(at.app),
                subject: Some(at.tool),
                source: Some(self.source.clone()),
                outcome: Some(outcome),
                duration_ms: Some(duration_ms),
                detail: Some(truncate(detail, 300)),
                input: Some(input_of(call)),
            },
        );

        if let Err(error) = recorded {
            eprintln!("Couldn't record a tool call in the log: {error}");
        }
    }
}

async fn record_calls(
    context: Arc<Context>,
    calls: Vec<RpcMessage>,
    headers: http::HeaderMap,
    copy: Body,
    started: i64,
) {
    let wanted: HashSet<String> = calls.iter().map(|call| id_key(call.id.as_ref())).collect();
    let mut found: HashMap<String, rpc::CallOutcome> = HashMap::new();
    let mut stream = payloads_within(&headers, copy, AUDIT_COPY_LIMIT);
    let mut broke: Option<String> = None;
    let collect = async {
        while found.len() < wanted.len() {
            let payload = match stream.next().await {
                Some(Ok(payload)) => payload,
                Some(Err(error)) => {
                    broke = Some(error.message);
                    break;
                }
                None => break,
            };

            for (id, outcome) in call_outcomes(&payload) {
                if wanted.contains(&id) {
                    found.entry(id).or_insert(outcome);
                }
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_mins(10), collect).await;
    let duration = now_ms() - started;

    for call in &calls {
        match found.get(&id_key(call.id.as_ref())) {
            Some(outcome) if outcome.ok => context.record_call(call, Outcome::Ok, duration, &outcome.message),
            Some(outcome) => context.record_call(call, Outcome::Error, duration, &outcome.message),
            None => match &broke {
                Some(message) if message == rpc::TOO_LARGE => {
                    context.record_call(call, Outcome::Ok, duration, "result too large to check");
                }
                Some(message) => context.record_call(call, Outcome::Error, duration, message),
                None => context.record_call(call, Outcome::Error, duration, "no result came back"),
            },
        }
    }
}

fn observed(response: Response, calls: Vec<RpcMessage>, context: Arc<Context>, started: i64) -> Response {
    if calls.is_empty() {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let (copy_sender, copy_receiver) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
    let teed = body.into_data_stream().map(move |chunk| {
        let chunk = chunk.map_err(std::io::Error::other);

        if let Ok(bytes) = &chunk {
            let _ = copy_sender.send(Ok(bytes.clone()));
        }

        chunk
    });
    let copy = Body::from_stream(UnboundedReceiverStream::new(copy_receiver));
    tokio::spawn(record_calls(context, calls, parts.headers.clone(), copy, started));

    Response::from_parts(parts, Body::from_stream(teed))
}

pub struct McpSurface {
    store: Store,
    session_key: String,
}

impl McpSurface {
    pub fn new(store: Store) -> Result<Self, StoreError> {
        let session_key = store.session_key()?;

        Ok(Self { store, session_key })
    }

    fn seal(&self, grant: &str, session: &str) -> String {
        format!("{session}.{}", hmac(&self.session_key, &format!("{grant}:{session}")))
    }

    fn unseal(&self, grant: &str, sealed: &str) -> Option<String> {
        let (session, _) = sealed.rsplit_once('.')?;

        (!session.is_empty() && safe_equal(sealed, &self.seal(grant, session))).then(|| session.to_owned())
    }

    fn seal_response(&self, grant: &str, mut response: Response) -> Response {
        let session = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()).map(str::to_owned);

        if let Some(session) = session
            && let Ok(value) = HeaderValue::from_str(&self.seal(grant, &session))
        {
            response.headers_mut().insert("mcp-session-id", value);
        }

        response
    }

    async fn post(&self, request: SourceRequest, context: Arc<Context>) -> Result<Response, SurfaceError> {
        let server = &context.server;
        let Some(decoded) = decode_batch::<RpcMessage>(&request.body) else {
            return Ok(parse_error());
        };
        let messages = decoded.items;

        if let Some((message, reason)) =
            messages.iter().find_map(|message| refusal(message, server).map(|reason| (message, reason)))
        {
            context.refuse("method.blocked", message.method.as_deref().unwrap_or("unknown"), None)?;
            return Ok(rpc::json(&rpc_error(message.id.as_ref(), -32601, &reason), StatusCode::OK));
        }

        if let Some(blocked) = messages.iter().find(|message| {
            message.method.as_deref() == Some("tools/call") && !server.allowed.contains(tool_name(message))
        }) {
            context.refuse("tool.blocked", tool_name(blocked), Some(input_of(blocked)))?;
            let message = format!("Tool not allowed: {}", tool_name(blocked));
            return Ok(rpc::json(&rpc_error(blocked.id.as_ref(), -32602, &message), StatusCode::OK));
        }

        if let Some(hidden) = messages
            .iter()
            .find(|message| message.method.as_deref() == Some("resources/read") && !readable(server, uri_of(message)))
        {
            let uri = uri_of(hidden);
            let subject = server.resources.get(uri).cloned().unwrap_or_else(|| uri.to_owned());
            context.refuse("tool.blocked", &subject, None)?;
            return Ok(rpc::json(&rpc_error(hidden.id.as_ref(), -32002, "Resource not found"), StatusCode::OK));
        }

        let started = now_ms();
        let calls: Vec<RpcMessage> = messages
            .iter()
            .filter(|message| match message.method.as_deref() {
                Some("tools/call") => true,
                Some("resources/read") => own_resource(uri_of(message)),
                _ => false,
            })
            .cloned()
            .collect();
        let response = match server.source.handle(request).await {
            Ok(response) => response,
            Err(error) => {
                let duration = now_ms() - started;

                for call in &calls {
                    context.record_call(call, Outcome::Error, duration, &error.message);
                }

                return Err(error.into());
            }
        };
        let method_listed = |name: &str| messages.iter().any(|message| message.method.as_deref() == Some(name));
        let tools_listed = if method_listed("tools/list") {
            let allowed = server.allowed.clone();
            let keep: Keep = Arc::new(move |tool| {
                tool.get("name").and_then(Value::as_str).is_some_and(|name| allowed.contains(name))
            });
            filter_list(response, ListKey::Tools, keep).await?
        } else {
            response
        };
        let listed = if method_listed("resources/list") {
            let link = server.clone();
            let keep: Keep = Arc::new(move |resource| {
                resource.get("uri").and_then(Value::as_str).is_some_and(|uri| readable(&link, uri))
            });
            filter_list(tools_listed, ListKey::Resources, keep).await?
        } else {
            tools_listed
        };

        Ok(observed(listed, calls, context.clone(), started))
    }

    pub async fn serve(&self, exchange: Exchange) -> Result<Response, SurfaceError> {
        let Exchange { mut request, server, grant, caller, source } = exchange;
        let sealed = request.header("mcp-session-id").map(str::to_owned);
        let session = sealed.as_deref().and_then(|sealed| self.unseal(&grant, sealed));

        if sealed.is_some() && session.is_none() {
            return Ok(rpc::json(&rpc_error(None, -32001, "Session not found"), StatusCode::NOT_FOUND));
        }

        if let Some(value) = session.and_then(|session| HeaderValue::from_str(&session).ok()) {
            request.headers.insert("mcp-session-id", value);
        }

        if server.state != ServerState::Live {
            let message = format!("{} isn't available yet: {}", server.name, server.detail);
            return Ok(rpc::json(&rpc_error(None, -32000, &message), StatusCode::SERVICE_UNAVAILABLE));
        }

        let response = if request.method == Method::POST {
            let context = Arc::new(Context { store: self.store.clone(), server, caller, source });
            self.post(request, context).await?
        } else {
            server.source.handle(request).await?
        };

        Ok(self.seal_response(&grant, response))
    }
}
