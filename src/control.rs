use crate::core::Problem;
use crate::core::in_state_dir;
use crate::gateway::ListenError;
use crate::links::ServerState;
use crate::registry::Registry;
use crate::rpc::json;
use crate::serve::{Daemon, Reload};
use crate::state::{DaemonState, view_of};
use crate::tunnels::tunnel_choice;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub fn socket_path() -> PathBuf {
    in_state_dir("control.sock")
}

pub struct Control {
    daemon: Arc<Daemon>,
    registry: Registry,
    reload: Arc<dyn Reload>,
}

pub async fn state_of(daemon: &Daemon, registry: &Registry, reload: &dyn Reload) -> DaemonState {
    let public_url = daemon.public_url();
    let refused: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|entry| entry.spec.source.report_refusal() && entry.state == crate::links::ServerState::Refused)
        .collect();

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
            .chain(refused.iter().map(|entry| Problem { app: entry.spec.name.clone(), message: entry.detail.clone() }))
            .collect(),
        refused: refused.into_iter().map(|entry| entry.spec.name).collect(),
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
        let risk = physical.spec.source.risk(found);

        if !try_allowed(
            physical.state,
            physical.allowed.contains(tool) && risk.claimed_read_only && risk.effect.is_none(),
            risk.runs_code,
            physical.spec.allow_dangerous,
        ) {
            return Err(format!("{tool} isn't available to try"));
        }

        let arguments = typed(&arguments, found.input_schema.as_ref());
        let reply = physical.source.call_tool(tool, arguments).await.map_err(|error| error.message)?;

        Ok(reply_text(&reply))
    }
}

fn try_allowed(state: ServerState, eligible: bool, dangerous: bool, allow_dangerous: bool) -> bool {
    state == ServerState::Live && eligible && (!dangerous || allow_dangerous)
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
        ensure_state_dir(dir).map_err(failed)?;
    }

    let listener = UnixListener::bind(path).map_err(failed)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ListenError { message: format!("Couldn't secure {}", path.display()) })?;
    let router = axum::Router::new().fallback(route).with_state(Arc::new(Control { daemon, registry, reload }));

    Ok(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };

            if !stream.peer_cred().is_ok_and(|credentials| credentials.uid() == nix::unistd::geteuid().as_raw()) {
                continue;
            }

            let app = router.clone();
            tokio::spawn(async move {
                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), hyper_util::service::TowerToHyperService::new(app));
                let _ = connection.await;
            });
        }
    }))
}

fn ensure_state_dir(dir: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dir)?;
    let metadata = std::fs::metadata(dir)?;

    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state directory belongs to another user",
        ));
    }

    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(())
}

pub fn remove_socket() {
    let _ = std::fs::remove_file(socket_path());
}

async fn call(path: &str, method: Method, body: Option<Value>, wait: Duration) -> Option<Value> {
    let exchange = async {
        let stream = UnixStream::connect(socket_path()).await.ok()?;
        if stream.peer_cred().ok()?.uid() != nix::unistd::geteuid().as_raw() {
            return None;
        }
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

#[cfg(test)]
mod tests {
    use super::{ensure_state_dir, try_allowed};
    use crate::links::ServerState;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn control_try_requires_a_live_allowed_read_only_tool_and_danger_permission() {
        assert!(try_allowed(ServerState::Live, true, false, false));
        assert!(!try_allowed(ServerState::Starting, true, false, false));
        assert!(!try_allowed(ServerState::Refused, true, false, false));
        assert!(!try_allowed(ServerState::Live, false, false, false));
        assert!(!try_allowed(ServerState::Live, true, true, false));
        assert!(try_allowed(ServerState::Live, true, true, true));
    }

    #[test]
    fn state_directory_permissions_are_tightened() {
        let directory = std::env::temp_dir().join(format!("porchlight-state-dir-{}", crate::crypto::random_token(6)));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_state_dir(&directory).unwrap();

        assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
