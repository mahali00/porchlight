use crate::approval::summarize;
use crate::auth::{ACCESS_TOKEN_SECONDS, Auth, AuthError, AuthorizationRequest, MAX_CODE_ATTEMPTS, TokenPair};
use crate::client_metadata::{ClientIdentity, ClientMetadata, is_valid_redirect_uri, redirect_matches};
use crate::core::{link_path, now_ms};
use crate::crypto::{random_token, sha256};
use crate::pages::{Entry, Message, authorize_page, ended_page, html_response, message_response, redirect_to};
use crate::registry::Registry;
use crate::rpc::{MAX_REQUEST_BODY, empty, form_params, is_event_stream, json, query_params, text};
use crate::serve::{Daemon, Shared};
use crate::sources::SourceRequest;
use crate::store::{AuditFields, Store};
use crate::surfaces::{Exchange, McpSurface, SurfaceError};
use crate::system;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ListenError {
    pub message: String,
}

pub async fn bind(port: u16) -> Result<TcpListener, ListenError> {
    TcpListener::bind(("127.0.0.1", port)).await.map_err(|_| ListenError { message: format!("Port {port} is in use") })
}

const CORS: [(&str, &str); 5] = [
    ("access-control-allow-origin", "*"),
    ("access-control-allow-methods", "GET, POST, DELETE, OPTIONS"),
    (
        "access-control-allow-headers",
        "authorization, content-type, accept, mcp-session-id, mcp-protocol-version, last-event-id, mcp-method, mcp-name",
    ),
    ("access-control-expose-headers", "www-authenticate, mcp-session-id, mcp-protocol-version"),
    ("access-control-max-age", "86400"),
];

fn cookie_name(request_id: &str) -> String {
    format!("__Host-pl_{request_id}")
}

pub fn source_of(headers: &HeaderMap) -> String {
    let local = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|host| matches!(host.rsplit_once(':').map_or(host, |(name, _)| name), "127.0.0.1" | "localhost"));
    let place = if local { "this computer" } else { "the tunnel" };
    let claimed: Option<String> = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next_back())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(64).collect());

    match claimed {
        Some(claimed) => format!("{place}, says {claimed}"),
        None => place.to_owned(),
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

struct Throttle {
    interval_ms: i64,
    last: Mutex<HashMap<String, i64>>,
}

fn refused(headers: &HeaderMap, body: &Value, message: &Message) -> Response {
    let browser = headers
        .get(header::ACCEPT)
        .and_then(|accept| accept.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));

    if browser { message_response(StatusCode::BAD_REQUEST, message) } else { json(body, StatusCode::BAD_REQUEST) }
}

impl Throttle {
    fn new(interval_ms: i64) -> Self {
        Self { interval_ms, last: Mutex::new(HashMap::new()) }
    }

    fn should_record(&self, key: &str) -> bool {
        let now = now_ms();

        self.last.lock().is_ok_and(|mut seen| {
            if now - seen.get(key).copied().unwrap_or_default() < self.interval_ms {
                return false;
            }

            if seen.len() > 10_000 {
                seen.retain(|_, at| now - *at < self.interval_ms);
            }

            seen.insert(key.to_owned(), now);
            true
        })
    }
}

#[derive(Deserialize)]
struct RegistrationRequest {
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
    client_name: Option<String>,
}

pub struct Gateway {
    daemon: Arc<Daemon>,
    store: Store,
    auth: Auth,
    registry: Registry,
    surface: McpSurface,
    client_metadata: ClientMetadata,
    per_source: Throttle,
    any_source: Throttle,
}

fn internal(error: &dyn std::fmt::Display) -> Response {
    eprintln!("{error}");
    text("Internal error", StatusCode::INTERNAL_SERVER_ERROR)
}

impl Gateway {
    pub fn new(daemon: Arc<Daemon>, shared: Shared) -> Result<Arc<Self>, crate::store::StoreError> {
        Ok(Arc::new(Self {
            daemon,
            surface: McpSurface::new(shared.store.clone())?,
            store: shared.store,
            auth: shared.auth,
            registry: shared.registry,
            client_metadata: shared.client_metadata,
            per_source: Throttle::new(1_000),
            any_source: Throttle::new(100),
        }))
    }

    pub fn serve(self: Arc<Self>, listener: TcpListener) -> tokio::task::JoinHandle<()> {
        let router = axum::Router::new().fallback(route).with_state(self);

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        })
    }

    fn protected_resource(&self, name: &str) -> Response {
        if self.registry.link(name).is_none() {
            return json(&json!({ "error": "unknown_resource" }), StatusCode::NOT_FOUND);
        }

        let public_url = self.daemon.public_url();

        json(
            &json!({
                "resource": format!("{public_url}{}", link_path(name)),
                "authorization_servers": [public_url],
                "bearer_methods_supported": ["header"],
            }),
            StatusCode::OK,
        )
    }

    fn authorization_server(&self) -> Response {
        let url = self.daemon.public_url();

        json(
            &json!({
                "issuer": url,
                "authorization_endpoint": format!("{url}/oauth/authorize"),
                "token_endpoint": format!("{url}/oauth/token"),
                "revocation_endpoint": format!("{url}/oauth/revoke"),
                "registration_endpoint": format!("{url}/oauth/register"),
                "code_challenge_methods_supported": ["S256"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "response_types_supported": ["code"],
                "token_endpoint_auth_methods_supported": ["none"],
                "client_id_metadata_document_supported": true,
                "authorization_response_iss_parameter_supported": true,
            }),
            StatusCode::OK,
        )
    }

    fn unauthorized(&self, name: &str) -> Response {
        let mut response = empty(StatusCode::UNAUTHORIZED);
        let challenge = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource{}\"",
            self.daemon.public_url(),
            link_path(name)
        );

        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
        }

        response
    }

    fn register(&self, headers: &HeaderMap, body: &[u8]) -> Result<Response, AuthError> {
        let Ok(registration) = serde_json::from_slice::<RegistrationRequest>(body) else {
            return Ok(json(&json!({ "error": "invalid_client_metadata" }), StatusCode::BAD_REQUEST));
        };

        if registration.redirect_uris.is_empty()
            || !registration.redirect_uris.iter().all(|uri| is_valid_redirect_uri(uri))
        {
            return Ok(json(&json!({ "error": "invalid_redirect_uri" }), StatusCode::BAD_REQUEST));
        }

        if registration.token_endpoint_auth_method.as_deref().is_some_and(|method| method != "none") {
            return Ok(json(&json!({ "error": "public_clients_only" }), StatusCode::BAD_REQUEST));
        }

        if !self.auth.allow_registration(&source_of(headers))? {
            return Ok(json(&json!({ "error": "rate_limited" }), StatusCode::TOO_MANY_REQUESTS));
        }
        let client_id = random_token(16);
        let name: Option<String> = registration
            .client_name
            .map(|name| name.trim().chars().take(80).collect())
            .filter(|name: &String| !name.is_empty());
        self.store.client_register(&client_id, &registration.redirect_uris, name.as_deref())?;
        self.store.audit_write("dcr.register", &client_id)?;

        Ok(json(
            &json!({ "client_id": client_id, "redirect_uris": registration.redirect_uris, "token_endpoint_auth_method": "none" }),
            StatusCode::CREATED,
        ))
    }

    async fn identify_client(&self, client_id: &str, redirect_uri: &str) -> Result<Option<ClientIdentity>, AuthError> {
        if let Some(registered) = self.store.client_redirect_uris(client_id)? {
            if !registered.iter().any(|uri| redirect_matches(uri, redirect_uri)) {
                return Ok(None);
            }

            return Ok(Some(ClientIdentity { name: self.store.client_name(client_id)?, verified_host: None }));
        }

        Ok(self
            .client_metadata
            .fetch(client_id)
            .await
            .filter(|document| document.redirect_uris.iter().any(|uri| redirect_matches(uri, redirect_uri)))
            .map(|document| ClientIdentity {
                name: document.name.unwrap_or_else(|| document.host.clone()),
                verified_host: Some(document.host),
            }))
    }

    async fn authorize(&self, headers: &HeaderMap, query: Option<&str>) -> Result<Response, AuthError> {
        let params = query_params(query);
        let get = |key: &str| params.get(key).cloned().unwrap_or_default();
        let challenge = get("code_challenge");

        if get("response_type") != "code" || challenge.is_empty() || get("code_challenge_method") != "S256" {
            return Ok(refused(
                headers,
                &json!({ "error": "invalid_request", "error_description": "response_type=code with S256 PKCE is required" }),
                &Message {
                    title: "This app can't connect this way",
                    detail: "It sent a connection request porchlight doesn't accept.",
                    next: "Update the app, then add the connector again.",
                },
            ));
        }

        let public_url = self.daemon.public_url();
        let resource = get("resource").trim_end_matches('/').to_owned();
        let Some(server) =
            self.registry.links().into_iter().find(|link| resource == format!("{public_url}{}", link_path(&link.name)))
        else {
            return Ok(refused(
                headers,
                &json!({ "error": "invalid_target" }),
                &Message {
                    title: "Nothing is shared at this address",
                    detail: "porchlight doesn't have a link at the address this app was given.",
                    next: "Check the address in your MCP client. porchlight lists each link's address under Links.",
                },
            ));
        };
        let client_id = get("client_id");
        let redirect_uri = get("redirect_uri");
        let Some(client) = self.identify_client(&client_id, &redirect_uri).await? else {
            return Ok(refused(
                headers,
                &json!({ "error": "invalid_redirect_uri" }),
                &Message {
                    title: "This app's details don't match",
                    detail: "It asked to send you somewhere it didn't register.",
                    next: "Remove the connector from your MCP client and add it again.",
                },
            ));
        };
        let request_id = random_token(12);
        let begun = self.auth.begin(
            AuthorizationRequest {
                id: request_id.clone(),
                client_id,
                server: server.name.clone(),
                redirect_uri: redirect_uri.clone(),
                challenge,
                state: get("state"),
            },
            &source_of(headers),
        );
        let nonce = match begun {
            Ok(nonce) => nonce,
            Err(AuthError::Refused(_)) => {
                return Ok(message_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    &Message {
                        title: "Too many connection requests",
                        detail: "Porchlight has received too many connection requests.",
                        next: "Wait a few minutes, then connect again from your MCP client.",
                    },
                ));
            }
            Err(error) => return Err(error),
        };
        let Some(pending) = self.store.pending_get(&request_id)? else {
            return Ok(ended_page(&client.name));
        };
        let summary = summarize(&self.registry, &pending, client).await;
        let message = format!(
            "{} wants to use {}. Code {} — type it on the device that's connecting.",
            summary.client.label(),
            summary.server_name,
            pending.code
        );
        tokio::spawn(async move { system::notify("porchlight", &message).await });
        let mut response = html_response(&authorize_page(&request_id, &summary, &Entry::Fresh), StatusCode::OK);
        let set_cookie = format!("{}={nonce}; Path=/; HttpOnly; Secure; SameSite=Lax", cookie_name(&request_id));

        if let Ok(value) = HeaderValue::from_str(&set_cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }

        Ok(response)
    }

    async fn complete_code(
        &self,
        headers: &HeaderMap,
        params: &HashMap<String, String>,
    ) -> Result<Response, AuthError> {
        let request_id = params.get("req").cloned().unwrap_or_default();
        let typed = params.get("code").cloned().unwrap_or_default();
        let nonce = cookie(headers, &cookie_name(&request_id));
        let live = |request_id: &str| -> Result<_, AuthError> {
            Ok(self.store.pending_get(request_id)?.filter(|pending| pending.expires_at >= now_ms()))
        };
        let Some(pending) = live(&request_id)? else {
            return Ok(ended_page("your MCP client"));
        };
        let client =
            self.client_metadata.identify(&pending.client_id, self.store.client_name(&pending.client_id)?).await;

        if nonce.as_deref().is_none_or(|nonce| sha256(nonce) != pending.nonce_hash) {
            let summary = summarize(&self.registry, &pending, client).await;
            return Ok(html_response(
                &authorize_page(&request_id, &summary, &Entry::OtherBrowser),
                StatusCode::FORBIDDEN,
            ));
        }

        match self.auth.approve_with_code(&request_id, &typed, nonce.as_deref()) {
            Ok(approval) => {
                let server = self.registry.link(&pending.server).map_or(pending.server, |server| server.name);
                let message = format!("{} is connected to {server}.", client.label());
                tokio::spawn(async move { system::notify("porchlight", &message).await });

                Ok(redirect_to(
                    &approval.redirect_uri,
                    &[("code", &approval.code), ("state", &approval.state), ("iss", &self.daemon.public_url())],
                ))
            }
            Err(AuthError::Refused(_)) => match live(&request_id)? {
                Some(pending) => {
                    let tries_left = MAX_CODE_ATTEMPTS - pending.attempts;
                    let summary = summarize(&self.registry, &pending, client).await;
                    Ok(html_response(
                        &authorize_page(&request_id, &summary, &Entry::Wrong { tries_left }),
                        StatusCode::FORBIDDEN,
                    ))
                }
                None => Ok(ended_page(&client.name)),
            },
            Err(error) => Err(error),
        }
    }

    fn token(&self, params: &HashMap<String, String>) -> Result<Response, AuthError> {
        let get = |key: &str| params.get(key).map(String::as_str).unwrap_or_default();
        let pair: Result<TokenPair, AuthError> = match get("grant_type") {
            "authorization_code" => self.auth.exchange_code(
                get("code"),
                get("code_verifier"),
                get("resource"),
                get("client_id"),
                get("redirect_uri"),
            ),
            "refresh_token" => self.auth.refresh(get("refresh_token")),
            _ => {
                return Ok(json(&json!({ "error": "unsupported_grant_type" }), StatusCode::BAD_REQUEST));
            }
        };

        match pair {
            Ok(pair) => Ok(json(
                &json!({
                    "access_token": pair.access,
                    "refresh_token": pair.refresh,
                    "token_type": "Bearer",
                    "expires_in": ACCESS_TOKEN_SECONDS,
                }),
                StatusCode::OK,
            )),
            Err(AuthError::Refused(_)) => Ok(json(&json!({ "error": "invalid_grant" }), StatusCode::BAD_REQUEST)),
            Err(error) => Err(error),
        }
    }

    fn revoke(&self, params: &HashMap<String, String>) -> Result<Response, AuthError> {
        if let Some(token) = params.get("token").filter(|token| !token.is_empty()) {
            self.auth.revoke(token)?;
        }

        Ok(empty(StatusCode::OK))
    }

    async fn serve_link(
        &self,
        method: Method,
        headers: HeaderMap,
        body: bytes::Bytes,
        name: &str,
        protocol: &str,
    ) -> Response {
        let Some(server) = self.registry.link(name).filter(|_| protocol == "mcp") else {
            return json(&json!({ "error": "unknown_server" }), StatusCode::NOT_FOUND);
        };
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let source = source_of(&headers);
        let grant = match &bearer {
            Some(bearer) => match self.auth.authorize(bearer, &server.name) {
                Ok(grant) => grant,
                Err(error) => return internal(&error),
            },
            None => None,
        };
        let (Some(bearer), Some(grant)) = (bearer, grant) else {
            if headers.contains_key(header::AUTHORIZATION)
                && self.per_source.should_record(&source)
                && self.any_source.should_record("")
            {
                let _ = self.store.audit_record(
                    "auth.rejected",
                    AuditFields {
                        app: Some(server.name.clone()),
                        source: Some(source),
                        detail: Some("unknown or expired token".to_owned()),
                        ..AuditFields::default()
                    },
                );
            }

            return self.unauthorized(&server.name);
        };
        let stream = self.auth.open_grant_stream(&grant);

        if !matches!(self.auth.still_allowed(&bearer, &server.name), Ok(Some(_))) {
            return self.unauthorized(&server.name);
        }

        let caller = self.auth.caller(&bearer).ok().flatten().unwrap_or_else(|| "an unknown client".to_owned());
        let source_app = server.source_app.clone();
        let link_name = server.name.clone();
        let exchange = Exchange { request: SourceRequest { method, headers, body }, server, grant, caller, source };

        match self.surface.serve(exchange).await {
            Ok(response) if is_event_stream(response.headers()) => {
                let (parts, body) = response.into_parts();
                let cancelled = stream.token.clone().cancelled_owned();
                let guarded = body.into_data_stream().take_until(cancelled).map(move |chunk| {
                    let _held = &stream;
                    chunk
                });
                Response::from_parts(parts, Body::from_stream(guarded))
            }
            Ok(response) => response,
            Err(SurfaceError::Upstream(error)) => {
                if let Some(app) = source_app {
                    self.registry.mark_down(&app);
                }

                text(&format!("{link_name} is unreachable: {}", error.message), StatusCode::BAD_GATEWAY)
            }
            Err(SurfaceError::Store(error)) => internal(&error),
        }
    }
}

fn with_cors(mut response: Response) -> Response {
    for (name, value) in CORS {
        response.headers_mut().insert(name, HeaderValue::from_static(value));
    }

    response
}

fn not_allowed() -> Response {
    text("Method not allowed", StatusCode::METHOD_NOT_ALLOWED)
}

fn settled(result: Result<Response, AuthError>) -> Response {
    result.unwrap_or_else(|error| internal(&error))
}

async fn route(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    with_cors(dispatch(&gateway, request).await)
}

async fn dispatch(gateway: &Gateway, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();

    if method == Method::OPTIONS {
        return empty(StatusCode::NO_CONTENT);
    }

    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return text("Payload Too Large", StatusCode::PAYLOAD_TOO_LARGE);
    };
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let headers = parts.headers;
    let is = |allowed: &Method| method == *allowed;

    match segments.as_slice() {
        [".well-known", "oauth-protected-resource", slug, "mcp"] => {
            if is(&Method::GET) {
                gateway.protected_resource(slug)
            } else {
                not_allowed()
            }
        }
        [".well-known", "oauth-authorization-server" | "openid-configuration"] => {
            if is(&Method::GET) {
                gateway.authorization_server()
            } else {
                not_allowed()
            }
        }
        ["oauth", "register"] => {
            if is(&Method::POST) {
                settled(gateway.register(&headers, &body))
            } else {
                not_allowed()
            }
        }
        ["oauth", "authorize"] => {
            if is(&Method::GET) {
                settled(gateway.authorize(&headers, query).await)
            } else {
                not_allowed()
            }
        }
        ["oauth", "complete-code"] => {
            if !is(&Method::POST) {
                return not_allowed();
            }

            settled(gateway.complete_code(&headers, &form_params(&headers, &body)).await)
        }
        ["oauth", "token"] => {
            if is(&Method::POST) {
                settled(gateway.token(&form_params(&headers, &body)))
            } else {
                not_allowed()
            }
        }
        ["oauth", "revoke"] => {
            if is(&Method::POST) {
                settled(gateway.revoke(&form_params(&headers, &body)))
            } else {
                not_allowed()
            }
        }
        [slug, protocol] if !slug.is_empty() && !protocol.is_empty() => {
            if is(&Method::GET) || is(&Method::POST) || is(&Method::DELETE) {
                gateway.serve_link(method, headers, body, slug, protocol).await
            } else {
                not_allowed()
            }
        }
        _ => text("Not found", StatusCode::NOT_FOUND),
    }
}
