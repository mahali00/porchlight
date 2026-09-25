use crate::auth::Auth;
use crate::client_metadata::ClientMetadata;
use crate::config::Config;
use crate::core::{ago, link_path, now_ms};
use crate::crypto::{random_token, sha256};
use crate::gateway::{form_params, query_params};
use crate::links::ServerState;
use crate::pages::{
    Message, Panel, PanelClient, PanelLink, PanelRequest, RequestSummary, SharedTool, approval_page, control_page,
    html_response, message_response, redirect_to,
};
use crate::policy::{is_dangerous, is_rule_pattern};
use crate::registry::Registry;
use crate::rpc::{empty, text};
use crate::serve::{Daemon, Reload, Shared};
use crate::store::{PendingRequest, Store, StoreError};
use crate::system;
use axum::extract::{Request, State};
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

const PANEL_TOKEN_TTL: i64 = 60 * 60 * 1000;

fn expired() -> Response {
    message_response(
        StatusCode::GONE,
        &Message {
            label: "expired",
            title: "this request expired",
            detail: "connection requests only last a few minutes, and this one ran out. nothing was shared.",
            next: "go back to your mcp client and connect again",
        },
    )
}

fn back() -> Response {
    let mut response = empty(StatusCode::SEE_OTHER);
    response.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/"));
    response
}

#[derive(Debug, thiserror::Error)]
enum PageError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
}

pub struct ApprovalPages {
    daemon: Arc<Daemon>,
    reload: Arc<dyn Reload>,
    store: Store,
    auth: Auth,
    registry: Registry,
    client_metadata: ClientMetadata,
    config: Config,
    host: String,
    origin: String,
    csrf_tokens: Mutex<HashMap<String, String>>,
    panel_tokens: Mutex<HashMap<String, i64>>,
}

impl ApprovalPages {
    pub fn new(daemon: Arc<Daemon>, reload: Arc<dyn Reload>, shared: Shared) -> Arc<Self> {
        let host = format!("127.0.0.1:{}", daemon.approval_port);

        Arc::new(Self {
            origin: format!("http://{host}"),
            host,
            daemon,
            reload,
            store: shared.store,
            auth: shared.auth,
            registry: shared.registry,
            client_metadata: shared.client_metadata,
            config: shared.config,
            csrf_tokens: Mutex::new(HashMap::new()),
            panel_tokens: Mutex::new(HashMap::new()),
        })
    }

    pub fn serve(self: Arc<Self>, listener: TcpListener) -> tokio::task::JoinHandle<()> {
        let router = axum::Router::new().fallback(route).with_state(self);

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        })
    }

    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let header_is =
            |name: &str, expected: &str| headers.get(name).and_then(|value| value.to_str().ok()) == Some(expected);

        header_is("origin", &self.origin) || header_is("sec-fetch-site", "same-origin")
    }

    fn panel_token(&self) -> String {
        let now = now_ms();
        let token = random_token(16);

        if let Ok(mut tokens) = self.panel_tokens.lock() {
            tokens.retain(|_, expires| *expires >= now);
            tokens.insert(token.clone(), now + PANEL_TOKEN_TTL);
        }

        token
    }

    fn panel_form(&self, headers: &HeaderMap, body: &[u8]) -> Option<HashMap<String, String>> {
        if !self.same_origin(headers) {
            return None;
        }

        let form = form_params(headers, body);
        let now = now_ms();
        let valid = form.get("csrf").is_some_and(|csrf| {
            self.panel_tokens.lock().is_ok_and(|tokens| tokens.get(csrf).is_some_and(|at| *at > now))
        });

        valid.then_some(form)
    }

    fn panel(&self) -> Result<Response, PageError> {
        let public_url = self.daemon.public_url();
        let grants = self.store.grants_list()?;
        let links = self
            .registry
            .links()
            .into_iter()
            .map(|link| PanelLink {
                address: format!("{public_url}{}", link_path(&link.name)),
                state: match link.state {
                    ServerState::Live => "live",
                    ServerState::Starting | ServerState::Waiting => "starting",
                    ServerState::Refused => "refused",
                },
                tools: link.allowed.len(),
                clients: grants.iter().filter(|grant| grant.server == link.name).count(),
                name: link.name,
            })
            .collect();
        let now = now_ms();
        let clients = grants
            .into_iter()
            .map(|grant| {
                Ok(PanelClient {
                    name: self.store.client_name(&grant.client_id)?,
                    id: grant.id,
                    link: grant.server,
                    last_used: ago(grant.last_used, now),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let pending = self
            .store
            .pending_list()?
            .into_iter()
            .filter(|request| request.expires_at > now)
            .map(|request| {
                Ok(PanelRequest {
                    client: self.store.client_name(&request.client_id)?,
                    id: request.id,
                    link: request.server,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let panel = Panel {
            host: system::host_name(),
            csrf: self.panel_token(),
            links,
            problems: self.reload.problems().into_iter().map(|problem| problem.message).collect(),
            pending,
            clients,
        };

        Ok(html_response(&control_page(&panel), StatusCode::OK))
    }

    async fn toggle(&self, headers: &HeaderMap, body: &[u8]) -> Result<Response, PageError> {
        let Some(form) = self.panel_form(headers, body) else {
            return Ok(text("Forbidden", StatusCode::FORBIDDEN));
        };
        let target = form.get("target").cloned().unwrap_or_default();
        let on = form.get("on").map(String::as_str) == Some("true");
        let settings = self.config.load()?.settings;

        if let Some(app) = target.strip_prefix("mcp:") {
            if !settings.mcp.contains_key(app) {
                return Ok(text("Bad request", StatusCode::BAD_REQUEST));
            }

            self.config.edit(&["mcp", app, "enabled"], Some(&Value::Bool(on)))?;
        } else {
            let mut parts = target.split('/');
            let app = parts.next().unwrap_or_default();
            let tool = parts.next().unwrap_or("*");
            let pattern = format!("{app}/{tool}");

            if !is_rule_pattern(&pattern) || parts.next().is_some() {
                return Ok(text("Bad request", StatusCode::BAD_REQUEST));
            }

            self.config.edit(&["tools", &pattern], Some(&Value::Bool(on)))?;
        }

        self.reload.reload().await;

        Ok(back())
    }

    fn revoke_client(&self, headers: &HeaderMap, body: &[u8]) -> Result<Response, PageError> {
        let Some(form) = self.panel_form(headers, body) else {
            return Ok(text("Forbidden", StatusCode::FORBIDDEN));
        };
        self.auth.revoke_grant(form.get("id").map(String::as_str).unwrap_or_default())?;

        Ok(back())
    }

    fn live_pending(&self, request_id: &str) -> Result<Option<PendingRequest>, StoreError> {
        Ok(self.store.pending_get(request_id)?.filter(|pending| pending.expires_at > now_ms()))
    }

    async fn show(&self, query: Option<&str>) -> Result<Response, PageError> {
        let request_id = query_params(query).get("req").cloned().unwrap_or_default();
        let Some(pending) = self.live_pending(&request_id)? else {
            return Ok(expired());
        };
        let csrf = random_token(16);

        if let Ok(mut tokens) = self.csrf_tokens.lock() {
            tokens.insert(request_id.clone(), csrf.clone());
        }

        let server = self.registry.link(&pending.server);
        let tools: Vec<SharedTool> = server
            .as_ref()
            .map(|server| {
                server
                    .tools
                    .iter()
                    .filter(|tool| server.allowed.contains(&tool.name))
                    .map(|tool| SharedTool { name: tool.name.clone(), dangerous: is_dangerous(tool) })
                    .collect()
            })
            .unwrap_or_default();
        let client =
            self.client_metadata.identify(&pending.client_id, self.store.client_name(&pending.client_id)?).await;
        let summary = RequestSummary {
            client,
            server_name: server.as_ref().map_or_else(|| pending.server.clone(), |server| server.name.clone()),
            tool_count: server.as_ref().map(|server| server.allowed.len()),
            redirect_uri: pending.redirect_uri.clone(),
        };

        Ok(html_response(
            &approval_page(&request_id, &summary, &tools, &system::host_name(), self.daemon.approval_port, &csrf),
            StatusCode::OK,
        ))
    }

    async fn decide(&self, headers: &HeaderMap, body: &[u8]) -> Result<Response, PageError> {
        if !self.same_origin(headers) {
            return Ok(text("Bad origin", StatusCode::FORBIDDEN));
        }

        let form = form_params(headers, body);
        let request_id = form.get("req").cloned().unwrap_or_default();
        let csrf = self.csrf_tokens.lock().ok().and_then(|mut tokens| tokens.remove(&request_id));

        if request_id.is_empty() || csrf.is_none() || csrf.as_ref() != form.get("csrf") {
            return Ok(text("Bad CSRF token", StatusCode::FORBIDDEN));
        }

        let Some(pending) = self.live_pending(&request_id)? else {
            return Ok(expired());
        };

        if form.get("decision").map(String::as_str) != Some("allow") {
            self.store.pending_remove(&request_id)?;
            self.store.audit_write("authz.denied", &request_id)?;

            return Ok(redirect_to(
                &pending.redirect_uri,
                &[("error", "access_denied"), ("state", &pending.state), ("iss", &self.daemon.public_url())],
            ));
        }

        let ticket = self.auth.issue_ticket(&request_id);
        let fingerprint: String = sha256(&ticket).chars().take(12).collect();
        self.store.audit_write("authz.ticket_issued", &format!("{request_id}:{fingerprint}"))?;
        let client =
            self.client_metadata.identify(&pending.client_id, self.store.client_name(&pending.client_id)?).await;
        let server_name = self.registry.link(&pending.server).map_or(pending.server, |server| server.name);
        system::notify("porchlight", &format!("{} can now use {server_name}.", client.name)).await;

        Ok(redirect_to(
            &format!("{}/oauth/complete", self.daemon.public_url()),
            &[("req", &request_id), ("g", &ticket)],
        ))
    }
}

async fn route(State(pages): State<Arc<ApprovalPages>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let host_ok = parts.headers.get(header::HOST).and_then(|value| value.to_str().ok()) == Some(pages.host.as_str());

    if !host_ok {
        return text("Forbidden", StatusCode::FORBIDDEN);
    }

    let Ok(body) = axum::body::to_bytes(body, crate::gateway::MAX_REQUEST_BODY).await else {
        return text("Payload Too Large", StatusCode::PAYLOAD_TOO_LARGE);
    };
    let headers = &parts.headers;
    let result = match (parts.method, parts.uri.path()) {
        (Method::GET, "/") => pages.panel(),
        (Method::GET, "/approve") => pages.show(parts.uri.query()).await,
        (Method::POST, "/approve") => pages.decide(headers, &body).await,
        (Method::POST, "/toggle") => pages.toggle(headers, &body).await,
        (Method::POST, "/revoke") => pages.revoke_client(headers, &body),
        _ => Ok(text("Not found", StatusCode::NOT_FOUND)),
    };

    result.unwrap_or_else(|error| {
        eprintln!("{error}");
        text("Internal error", StatusCode::INTERNAL_SERVER_ERROR)
    })
}
