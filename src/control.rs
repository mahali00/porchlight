use crate::config::in_state_dir;
use crate::core::Problem;
use crate::gateway::ListenError;
use crate::registry::Registry;
use crate::rpc::json;
use crate::serve::{Daemon, Reload, ServerView, view_of};
use crate::tunnels::tunnel_choice;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub fn socket_path() -> PathBuf {
    in_state_dir("control.sock")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonState {
    pub pid: u32,
    pub tunnel: String,
    pub public_url: String,
    pub servers: Vec<ServerView>,
    pub problems: Vec<Problem>,
}

pub struct Control {
    daemon: Arc<Daemon>,
    registry: Registry,
    reload: Arc<dyn Reload>,
}

pub async fn state_of(daemon: &Daemon, registry: &Registry, reload: &dyn Reload) -> DaemonState {
    let public_url = daemon.public_url();
    let refused = registry
        .list()
        .into_iter()
        .filter(|entry| entry.spec.source.report_refusal() && entry.state == crate::links::ServerState::Refused);

    DaemonState {
        pid: std::process::id(),
        tunnel: daemon
            .provider()
            .await
            .map_or_else(|| "your own tunnel".to_owned(), |provider| provider.name().to_owned()),
        servers: registry.links().iter().map(|entry| view_of(entry, &public_url)).collect(),
        problems: reload
            .problems()
            .into_iter()
            .chain(refused.map(|entry| Problem { app: entry.spec.name, message: entry.detail }))
            .collect(),
        public_url,
    }
}

impl Control {
    async fn state(&self) -> DaemonState {
        state_of(&self.daemon, &self.registry, self.reload.as_ref()).await
    }

    async fn try_tool(&self, app: &str, tool: &str, arguments: Value) -> Result<(bool, String), String> {
        let physical = self.registry.get(app).ok_or_else(|| format!("{app} isn't running"))?;
        let found = physical
            .tools
            .iter()
            .find(|candidate| candidate.name == tool)
            .ok_or_else(|| format!("{app} has no tool called {tool}"))?;
        let read_only = found.annotations.as_ref().and_then(|annotations| annotations.read_only_hint) == Some(true);

        if !read_only {
            return Err(format!("{tool} may change things, so porchlight only runs it when a client calls it"));
        }

        let arguments = typed(&arguments, found.input_schema.as_ref());
        let reply = physical.source.call_tool(tool, arguments).await.map_err(|error| error.message)?;

        Ok(reply_text(&reply))
    }
}

fn state_response(state: &DaemonState) -> Response {
    json(&serde_json::to_value(state).unwrap_or(Value::Null), StatusCode::OK)
}

async fn route(State(control): State<Arc<Control>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();

    match (parts.method, parts.uri.path()) {
        (Method::GET, "/state") => state_response(&control.state().await),
        (Method::POST, "/reload") => {
            control.reload.reload().await;
            control.registry.settle_all().await;
            state_response(&control.state().await)
        }
        (Method::POST, "/tunnel") => {
            let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
            let choice = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| value.get("tunnel").and_then(Value::as_str).map(str::to_owned))
                .filter(|choice| tunnel_choice(choice).is_some());
            let Some(choice) = choice else {
                return json(
                    &json!({ "error": "Use a tunnel provider name or an https:// URL" }),
                    StatusCode::BAD_REQUEST,
                );
            };

            match control.daemon.set_tunnel(&choice).await {
                Ok(_) => state_response(&control.state().await),
                Err(error) => json(&json!({ "error": error.message }), StatusCode::BAD_GATEWAY),
            }
        }
        (Method::POST, "/try") => {
            let bytes = axum::body::to_bytes(body, 256 * 1024).await.unwrap_or_default();
            let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let field = |name: &str| asked.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
            let (app, tool) = (field("app"), field("tool"));
            let arguments = asked.get("arguments").cloned().unwrap_or_else(|| json!({}));

            match control.try_tool(&app, &tool, arguments).await {
                Ok((ok, text)) => json(&json!({ "ok": ok, "text": text }), StatusCode::OK),
                Err(error) => json(&json!({ "error": error }), StatusCode::BAD_REQUEST),
            }
        }
        _ => json(&json!({ "error": "Not found" }), StatusCode::NOT_FOUND),
    }
}

fn typed(arguments: &Value, schema: Option<&Value>) -> Value {
    let kind = |name: &str| {
        schema
            .and_then(|schema| schema.pointer(&format!("/properties/{name}/type")))
            .and_then(Value::as_str)
            .unwrap_or("string")
    };
    let values: serde_json::Map<String, Value> = arguments
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            let text = value.as_str()?;
            let parsed = match kind(name) {
                "string" => json!(text),
                _ => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
            };
            (!text.is_empty()).then(|| (name.clone(), parsed))
        })
        .collect();

    Value::Object(values)
}

fn reply_text(reply: &Value) -> (bool, String) {
    if let Some(message) = reply.pointer("/error/message").and_then(Value::as_str) {
        return (false, message.to_owned());
    }

    let result = reply.get("result").cloned().unwrap_or(Value::Null);
    let failed = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let text: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| match item.get("text").and_then(Value::as_str) {
            Some(text) => text.to_owned(),
            None => format!("[{}]", item.get("type").and_then(Value::as_str).unwrap_or("content")),
        })
        .collect();
    let text = if text.is_empty() {
        result.get("structuredContent").map(Value::to_string).unwrap_or_default()
    } else {
        text.join("\n")
    };

    (!failed, text)
}

pub fn serve(
    path: &Path,
    daemon: Arc<Daemon>,
    registry: Registry,
    reload: Arc<dyn Reload>,
) -> Result<tokio::task::JoinHandle<()>, ListenError> {
    let failed = |_| ListenError { message: format!("Couldn't open {}", path.display()) };
    let _ = std::fs::remove_file(path);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }

    let listener = UnixListener::bind(path).map_err(failed)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ListenError { message: format!("Couldn't secure {}", path.display()) })?;
    let router = axum::Router::new().fallback(route).with_state(Arc::new(Control { daemon, registry, reload }));

    Ok(tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    }))
}

pub fn remove_socket() {
    let _ = std::fs::remove_file(socket_path());
}

async fn call(path: &str, method: Method, body: Option<Value>, wait: Duration) -> Option<Value> {
    let exchange = async {
        let stream = UnixStream::connect(socket_path()).await.ok()?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.ok()?;
        tokio::spawn(connection);
        let request = http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "porchlight")
            .header("content-type", "application/json")
            .body(Body::from(body.map(|body| body.to_string()).unwrap_or_default()))
            .ok()?;
        let response = sender.send_request(request).await.ok()?;
        let bytes = response.into_body().collect().await.ok()?.to_bytes();

        serde_json::from_slice(&bytes).ok()
    };

    tokio::time::timeout(wait, exchange).await.ok().flatten()
}

fn decode(value: Option<Value>) -> Option<DaemonState> {
    serde_json::from_value(value?).ok()
}

pub async fn daemon_state() -> Option<DaemonState> {
    decode(call("/state", Method::GET, None, Duration::from_secs(20)).await)
}

pub async fn reload_daemon() -> Option<DaemonState> {
    decode(call("/reload", Method::POST, None, Duration::from_secs(20)).await)
}

pub async fn set_daemon_tunnel(tunnel: &str) -> Result<DaemonState, String> {
    let reply = call("/tunnel", Method::POST, Some(json!({ "tunnel": tunnel })), Duration::from_secs(20)).await;
    let error = reply.as_ref().and_then(|value| value.get("error")).and_then(Value::as_str).map(str::to_owned);

    match (decode(reply), error) {
        (Some(state), _) => Ok(state),
        (None, Some(error)) => Err(error),
        (None, None) => Err("porchlight isn't running".to_owned()),
    }
}

pub async fn try_on_daemon(app: &str, tool: &str, arguments: &Value) -> Result<(bool, String), String> {
    let asked = json!({ "app": app, "tool": tool, "arguments": arguments });
    let reply = call("/try", Method::POST, Some(asked), Duration::from_secs(70))
        .await
        .ok_or_else(|| "porchlight isn't running, or the tool took too long".to_owned())?;

    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        return Err(error.to_owned());
    }

    Ok((
        reply.get("ok").and_then(Value::as_bool).unwrap_or(false),
        reply.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
    ))
}
