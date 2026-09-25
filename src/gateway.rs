use crate::auth::{ACCESS_TOKEN_SECONDS, Approval, Auth, AuthError, AuthorizationRequest, TokenPair};
use crate::client_metadata::{ClientIdentity, ClientMetadata, is_valid_redirect_uri, redirect_matches};
use crate::core::{link_path, now_ms};
use crate::crypto::random_token;
use crate::links::ServerState;
use crate::pages::{Message, RequestSummary, authorize_page, html_response, message_response, redirect_to};
use crate::registry::Registry;
use crate::rpc::{empty, is_event_stream, json, text};
use crate::serve::{Daemon, Shared};
use crate::sources::SourceRequest;
use crate::store::{AuditFields, Store};
use crate::surfaces::{Exchange, McpSurface, SurfaceError};
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

pub const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

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
        "authorization, content-type, accept, mcp-session-id, mcp-protocol-version, last-event-id",
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

pub fn query_params(query: Option<&str>) -> HashMap<String, String> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes()).into_owned().collect()
}

pub fn form_params(headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));

    if is_json {
        return serde_json::from_slice::<serde_json::Map<String, Value>>(body)
            .map(|fields| {
                fields
                    .into_iter()
                    .filter_map(|(key, value)| value.as_str().map(|text| (key, text.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
    }

    form_urlencoded::parse(body).into_owned().collect()
}

struct Throttle {
    interval_ms: i64,
    last: Mutex<HashMap<String, i64>>,
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
        if !self.auth.allow_registration(&source_of(headers))? {
            return Ok(json(&json!({ "error": "rate_limited" }), StatusCode::TOO_MANY_REQUESTS));
        }

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
            return Ok(json(
                &json!({ "error": "invalid_request", "error_description": "response_type=code with S256 PKCE is required" }),
                StatusCode::BAD_REQUEST,
            ));
        }

        let public_url = self.daemon.public_url();
        let resource = get("resource").trim_end_matches('/').to_owned();
        let Some(server) =
            self.registry.links().into_iter().find(|link| resource == format!("{public_url}{}", link_path(&link.name)))
        else {
            return Ok(json(&json!({ "error": "invalid_target" }), StatusCode::BAD_REQUEST));
        };
        let client_id = get("client_id");
        let redirect_uri = get("redirect_uri");
        let Some(client) = self.identify_client(&client_id, &redirect_uri).await? else {
            return Ok(json(&json!({ "error": "invalid_redirect_uri" }), StatusCode::BAD_REQUEST));
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
                        label: "slow down",
                        title: "too many connection requests",
                        detail: "porchlight limits new requests so nobody can guess their way in.",
                        next: "wait a few minutes, then connect again from your mcp client",
                    },
                ));
            }
            Err(error) => return Err(error),
        };
        let summary = RequestSummary {
            client,
            server_name: server.name.clone(),
            tool_count: (server.state == ServerState::Live).then_some(server.allowed.len()),
            redirect_uri,
        };
        let mut response =
            html_response(&authorize_page(&request_id, &summary, self.daemon.approval_port), StatusCode::OK);
        let set_cookie = format!("{}={nonce}; Path=/; HttpOnly; Secure; SameSite=Lax", cookie_name(&request_id));

        if let Ok(value) = HeaderValue::from_str(&set_cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }

        Ok(response)
    }

    fn complete(
        &self,
        headers: &HeaderMap,
        params: &HashMap<String, String>,
        secret: &str,
        approve: impl Fn(&Auth, &str, &str, Option<&str>) -> Result<Approval, AuthError>,
        failure: &Message,
    ) -> Result<Response, AuthError> {
        let request_id = params.get("req").cloned().unwrap_or_default();
        let nonce = cookie(headers, &cookie_name(&request_id));
        let given = params.get(secret).cloned().unwrap_or_default();

        match approve(&self.auth, &request_id, &given, nonce.as_deref()) {
            Ok(approval) => Ok(redirect_to(
                &approval.redirect_uri,
                &[("code", &approval.code), ("state", &approval.state), ("iss", &self.daemon.public_url())],
            )),
            Err(AuthError::Refused(_)) => Ok(message_response(StatusCode::FORBIDDEN, failure)),
            Err(error) => Err(error),
        }
    }

    fn token(&self, params: &HashMap<String, String>) -> Result<Response, AuthError> {
        let get = |key: &str| params.get(key).map(String::as_str).unwrap_or_default();
        let pair: Result<TokenPair, AuthError> = match get("grant_type") {
            "authorization_code" => self.auth.exchange_code(get("code"), get("code_verifier"), get("resource")),
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
        ["oauth", "complete"] => {
            if !is(&Method::GET) {
                return not_allowed();
            }

            let failure = Message {
                label: "approval failed",
                title: "approval failed",
                detail: "each approval link works once, and this one was already used or ran out.",
                next: "go back to your mcp client and connect again",
            };
            settled(gateway.complete(&headers, &query_params(query), "g", Auth::approve_with_ticket, &failure))
        }
        ["oauth", "complete-code"] => {
            if !is(&Method::POST) {
                return not_allowed();
            }

            let failure = Message {
                label: "wrong code",
                title: "that code didn't work",
                detail: "each code works once and only for a few minutes. check for a typo, or get a fresh one.",
                next: "open porchlight on the computer and get a new code from the clients tab",
            };
            settled(gateway.complete(
                &headers,
                &form_params(&headers, &body),
                "code",
                Auth::approve_with_code,
                &failure,
            ))
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
