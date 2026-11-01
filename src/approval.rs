use crate::auth::Auth;
use crate::client_metadata::{ClientIdentity, ClientMetadata};
use crate::core::{link_path, now_ms};
use crate::crypto::random_token;
use crate::pages::{
    Message, Outcome, Panel, PanelLink, PanelRequest, RequestSummary, SharedTool, code_page, control_page,
    html_response, message_response, outcome_page,
};
use crate::policy::Risk;
use crate::registry::Registry;
use crate::rpc::text;
use crate::rpc::{MAX_REQUEST_BODY, form_params, query_params};
use crate::serve::Daemon;
use crate::serve::Shared;
use crate::sources::announced_risk;
use crate::store::{PendingRequest, Store, StoreError};
use crate::system;
use axum::extract::{Request, State};
use axum::response::Response;
use http::{HeaderMap, Method, StatusCode, header};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

fn expired() -> Response {
    message_response(
        StatusCode::GONE,
        &Message {
            title: "This request has ended",
            detail: "It expired, or it was already answered.",
            next: "Requests that are still waiting are on the porchlight home page.",
        },
    )
}

pub async fn summarize(registry: &Registry, pending: &PendingRequest, client: ClientIdentity) -> RequestSummary {
    let server = registry.link(&pending.server);
    let tools = server
        .as_ref()
        .map(|server| {
            server
                .tools
                .iter()
                .filter(|tool| server.allowed.contains(&tool.name))
                .map(|tool| SharedTool {
                    name: tool.name.clone(),
                    level: server.risks.get(&tool.name).map_or_else(|| announced_risk(tool).level(), Risk::level),
                })
                .collect()
        })
        .unwrap_or_default();

    RequestSummary {
        client,
        server_name: server.map_or_else(|| pending.server.clone(), |server| server.name),
        computer: system::host_name(),
        tools,
        opened_at: pending.created_at,
        expires_at: pending.expires_at,
        redirect_uri: pending.redirect_uri.clone(),
    }
}

struct Seen {
    client_id: String,
    client: String,
    server: String,
}

#[derive(Debug, thiserror::Error)]
enum PageError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthError),
}

pub struct ApprovalPages {
    daemon: Arc<Daemon>,
    store: Store,
    auth: Auth,
    registry: Registry,
    client_metadata: ClientMetadata,
    host: String,
    origin: String,
    csrf_tokens: Mutex<HashMap<String, (String, i64)>>,
    seen: Mutex<HashMap<String, Seen>>,
}

impl ApprovalPages {
    pub fn new(daemon: Arc<Daemon>, shared: Shared) -> Arc<Self> {
        let host = format!("127.0.0.1:{}", daemon.approval_port);

        Arc::new(Self {
            origin: format!("http://{host}"),
            host,
            daemon,
            store: shared.store,
            auth: shared.auth,
            registry: shared.registry,
            client_metadata: shared.client_metadata,
            csrf_tokens: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
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
    fn panel(&self) -> Result<Response, PageError> {
        let public_url = self.daemon.public_url();
        let links = self
            .registry
            .links()
            .into_iter()
            .map(|link| PanelLink { address: format!("{public_url}{}", link_path(&link.name)), name: link.name })
            .collect();
        let now = now_ms();
        let pending = self
            .store
            .pending_list()?
            .into_iter()
            .filter(|request| request.expires_at > now)
            .map(|request| {
                Ok(PanelRequest {
                    client: self.store.client_label(&request.client_id)?,
                    id: request.id,
                    link: request.server,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let panel = Panel { links, pending };

        Ok(html_response(&control_page(&panel), StatusCode::OK))
    }
    fn live_pending(&self, request_id: &str) -> Result<Option<PendingRequest>, StoreError> {
        Ok(self.store.pending_get(request_id)?.filter(|pending| pending.expires_at > now_ms()))
    }

    fn outcome(&self, request_id: &str) -> Result<Response, PageError> {
        let Some((client, server, client_id)) = self.seen.lock().ok().and_then(|seen| {
            seen.get(request_id).map(|seen| (seen.client.clone(), seen.server.clone(), seen.client_id.clone()))
        }) else {
            return Ok(expired());
        };
        let connected = self.auth.was_approved(request_id)
            || self.store.grants_list()?.iter().any(|grant| grant.client_id == client_id && grant.server == server);
        let outcome = if connected { Outcome::Connected } else { Outcome::Ended };

        Ok(html_response(&outcome_page(&client, &server, &outcome), StatusCode::OK))
    }

    fn csrf_for(&self, request_id: &str) -> String {
        let now = now_ms();
        self.csrf_tokens
            .lock()
            .map(|mut tokens| {
                tokens.retain(|_, (_, expires_at)| *expires_at > now);
                tokens
                    .entry(request_id.to_owned())
                    .or_insert_with(|| (random_token(16), now + 10 * 60 * 1000))
                    .0
                    .clone()
            })
            .unwrap_or_default()
    }

    async fn show(&self, query: Option<&str>) -> Result<Response, PageError> {
        let request_id = query_params(query).get("req").cloned().unwrap_or_default();
        let Some(pending) = self.live_pending(&request_id)? else {
            return self.outcome(&request_id);
        };
        let client =
            self.client_metadata.identify(&pending.client_id, self.store.client_name(&pending.client_id)?).await;
        let summary = summarize(&self.registry, &pending, client).await;

        if let Ok(mut seen) = self.seen.lock() {
            seen.retain(|id, _| self.store.pending_get(id).ok().flatten().is_some() || id == &request_id);
            seen.insert(
                request_id.clone(),
                Seen {
                    client_id: pending.client_id.clone(),
                    client: summary.client.name.clone(),
                    server: summary.server_name.clone(),
                },
            );
        }

        Ok(html_response(
            &code_page(&request_id, &summary, &pending.code, pending.attempts, &self.csrf_for(&request_id)),
            StatusCode::OK,
        ))
    }

    fn deny(&self, headers: &HeaderMap, body: &[u8]) -> Result<Response, PageError> {
        if !self.same_origin(headers) {
            return Ok(text("Bad origin", StatusCode::FORBIDDEN));
        }

        let form = form_params(headers, body);
        let request_id = form.get("req").cloned().unwrap_or_default();
        let csrf = self
            .csrf_tokens
            .lock()
            .ok()
            .and_then(|mut tokens| tokens.remove(&request_id))
            .filter(|(_, expires_at)| *expires_at > now_ms())
            .map(|(csrf, _)| csrf);

        if request_id.is_empty() || csrf.is_none() || csrf.as_ref() != form.get("csrf") {
            return Ok(text("Bad CSRF token", StatusCode::FORBIDDEN));
        }

        let Some(pending) = self.live_pending(&request_id)? else {
            return self.outcome(&request_id);
        };

        self.store.pending_remove(&request_id)?;
        self.store.audit_write("authz.denied", &request_id)?;
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(&request_id);
        }
        let client = self.store.client_name(&pending.client_id)?;
        let server = self.registry.link(&pending.server).map_or(pending.server, |server| server.name);

        Ok(html_response(&outcome_page(&client, &server, &Outcome::Denied), StatusCode::OK))
    }
}

async fn route(State(pages): State<Arc<ApprovalPages>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let host_ok = parts.headers.get(header::HOST).and_then(|value| value.to_str().ok()) == Some(pages.host.as_str());

    if !host_ok {
        return text("Forbidden", StatusCode::FORBIDDEN);
    }

    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return text("Payload Too Large", StatusCode::PAYLOAD_TOO_LARGE);
    };
    let headers = &parts.headers;
    let result = match (parts.method, parts.uri.path()) {
        (Method::GET, "/") => pages.panel(),
        (Method::GET, "/approve") => pages.show(parts.uri.query()).await,
        (Method::POST, "/deny") => pages.deny(headers, &body),
        _ => Ok(text("Not found", StatusCode::NOT_FOUND)),
    };

    result.unwrap_or_else(|error| {
        eprintln!("{error}");
        text("Internal error", StatusCode::INTERNAL_SERVER_ERROR)
    })
}
