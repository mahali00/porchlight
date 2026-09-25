#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use futures::StreamExt;
use http::{HeaderMap, StatusCode};
use indexmap::IndexMap;
use porchlight::approval::ApprovalPages;
use porchlight::apps::{AppSpec, apps_from, host_environment};
use porchlight::auth::Auth;
use porchlight::client_metadata::ClientMetadata;
use porchlight::config::{Config, Settings, decode_command_app};
use porchlight::core::Problem;
use porchlight::crypto::{pkce_challenge, random_token};
use porchlight::gateway::{Gateway, bind};
use porchlight::links::{LinkPlan, ServerState};
use porchlight::policy::rules_from;
use porchlight::registry::Registry;
use porchlight::serve::{Daemon, Reload, Shared};
use porchlight::sources::SourceSpec;
use porchlight::store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Upstream {
    received: Mutex<Vec<HeaderMap>>,
    landed: AtomicUsize,
    dynamic: Mutex<Vec<Value>>,
}

fn upstream_tools(path: &str, upstream: &Upstream) -> Value {
    match path {
        "/shell/mcp" => json!([{ "name": "run_shell" }]),
        "/dynamic/mcp" => Value::Array(upstream.dynamic.lock().unwrap().clone()),
        _ => json!([
            { "name": "get_basic_info", "annotations": { "readOnlyHint": true } },
            { "name": "write_html" },
            { "name": "delete_nodes" },
        ]),
    }
}

async fn upstream_route(State(upstream): State<Arc<Upstream>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();

    if path == "/landing" {
        upstream.landed.fetch_add(1, Ordering::SeqCst);
        return Response::new(Body::from("landed"));
    }

    upstream.received.lock().unwrap().push(parts.headers.clone());

    if parts.method == http::Method::DELETE {
        return Response::new(Body::empty());
    }

    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let message: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    if message.to_string().contains("redirect_me") {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "/landing")
            .body(Body::empty())
            .unwrap();
    }

    if message["method"] == "notifications/initialized" {
        return Response::builder().status(StatusCode::ACCEPTED).body(Body::empty()).unwrap();
    }

    if message["method"] == "initialize" {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": { "protocolVersion": "2025-06-18", "capabilities": { "tools": { "listChanged": true } }, "serverInfo": { "name": "test", "version": "1" } },
        });
        return Response::builder()
            .header("content-type", "application/json")
            .header("mcp-session-id", "upstream-1")
            .body(Body::from(reply.to_string()))
            .unwrap();
    }

    let tools = upstream_tools(&path, &upstream);

    if let Value::Array(batch) = &message {
        let replies: Vec<Value> = batch
            .iter()
            .enumerate()
            .map(|(id, _)| json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }))
            .collect();
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(Value::Array(replies).to_string()))
            .unwrap();
    }

    let reply = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": tools, "echo": message } });
    let body = if path == "/split/mcp" { serde_json::to_string_pretty(&reply).unwrap() } else { reply.to_string() };
    let data: Vec<String> = body.lines().map(|line| format!("data: {line}")).collect();

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(format!("event: message\n{}\n\n", data.join("\n"))))
        .unwrap()
}

async fn serve_upstream() -> (Arc<Upstream>, u16) {
    let upstream = Arc::new(Upstream::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = axum::Router::new().fallback(upstream_route).with_state(upstream.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    (upstream, port)
}

async fn stoppable_upstream() -> (tokio::sync::oneshot::Sender<()>, u16) {
    let upstream = Arc::new(Upstream::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = axum::Router::new().fallback(upstream_route).with_state(upstream);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
    });

    (stop, port)
}

struct NoReload;

impl Reload for NoReload {
    fn reload(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn problems(&self) -> Vec<Problem> {
        Vec::new()
    }
}

fn http_spec(name: &str, url: &str, allow_dangerous: bool) -> AppSpec {
    AppSpec {
        name: name.to_owned(),
        source: SourceSpec::Http { url: url.to_owned(), headers: IndexMap::new() },
        allow_dangerous,
    }
}

fn fixture_spec() -> AppSpec {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio-server.sh");

    AppSpec {
        name: "fixture".to_owned(),
        source: SourceSpec::Stdio {
            app: "fixture".to_owned(),
            command: vec![script.display().to_string()],
            env: IndexMap::new(),
            cwd: None,
        },
        allow_dangerous: false,
    }
}

fn command_spec(name: &str, app: &Value) -> AppSpec {
    let settings = Settings {
        commands: IndexMap::from([(name.to_owned(), decode_command_app(app).unwrap())]),
        ..Settings::default()
    };

    apps_from(&settings, &host_environment(), None).specs.remove(0)
}

fn shared_rules(extra: &[(&str, bool)]) -> porchlight::policy::Rules {
    let mut rules: IndexMap<String, bool> = [
        ("paper/delete_nodes", false),
        ("split/delete_nodes", false),
        ("allowlist/*", false),
        ("allowlist/write_html", true),
        ("fixture/wipe", false),
    ]
    .into_iter()
    .map(|(pattern, on)| (pattern.to_owned(), on))
    .collect();
    rules.extend(extra.iter().map(|(pattern, on)| ((*pattern).to_owned(), *on)));

    rules_from(&rules)
}

fn direct(name: &str) -> LinkPlan {
    LinkPlan::Direct { name: name.to_owned(), app: name.to_owned() }
}

fn base_links() -> Vec<LinkPlan> {
    let mut links: Vec<LinkPlan> = ["paper", "split", "allowlist", "fixture"].into_iter().map(direct).collect();
    links.push(LinkPlan::Commands {
        name: "home".to_owned(),
        apps: vec!["shelf".to_owned()],
        membership: vec!["shelf".to_owned()],
    });
    links.push(LinkPlan::Commands {
        name: "vault".to_owned(),
        apps: vec!["vault".to_owned()],
        membership: vec!["vault".to_owned()],
    });
    links
}

async fn reaches(registry: &Registry, name: &str, state: ServerState) {
    let mut changes = registry.changes(name).unwrap();
    tokio::time::timeout(Duration::from_secs(10), changes.wait_for(|entry| entry.state == state))
        .await
        .unwrap_or_else(|_| panic!("{name} never became {state:?}"))
        .unwrap();
}

struct Harness {
    registry: Registry,
    auth: Auth,
    store: Store,
    upstream: Arc<Upstream>,
    upstream_port: u16,
    gateway: String,
    approval: String,
    approval_port: u16,
    config_path: std::path::PathBuf,
    tokens: HashMap<String, String>,
    expired: String,
    client: reqwest::Client,
    daemon: Arc<Daemon>,
    shared: Shared,
}

impl Harness {
    async fn start() -> Self {
        let (upstream, upstream_port) = serve_upstream().await;
        let store = Store::in_memory().unwrap();
        let auth = Auth::new(store.clone());
        let registry = Registry::new(store.clone(), "test-host".to_owned());
        registry.set_rules(shared_rules(&[])).await;
        let url = |path: &str| format!("http://127.0.0.1:{upstream_port}{path}");
        let specs = vec![
            http_spec("paper", &url("/mcp"), false),
            http_spec("split", &url("/split/mcp"), false),
            http_spec("allowlist", &url("/mcp"), false),
            fixture_spec(),
            command_spec(
                "shelf",
                &json!({
                    "door": { "run": "echo shut", "resource": true },
                    "say": "echo said {words}",
                    "light": { "run": "echo light {room}", "inputs": { "room": { "choices": ["kitchen"] } } },
                }),
            ),
            command_spec("vault", &json!({ "secret": { "run": "echo s3cret", "resource": true } })),
        ];
        registry.sync(specs, base_links()).await;

        for name in ["paper", "split", "allowlist", "fixture", "shelf"] {
            if registry.get(name).unwrap().state != ServerState::Live {
                reaches(&registry, name, ServerState::Live).await;
            }
        }

        let gateway_listener = bind(0).await.unwrap();
        let approval_listener = bind(0).await.unwrap();
        let gateway_port = gateway_listener.local_addr().unwrap().port();
        let approval_port = approval_listener.local_addr().unwrap().port();
        let daemon = Daemon::start(gateway_port, approval_port, "https://tunnel.example").await.unwrap();
        let dir = std::env::temp_dir().join(format!("porchlight-servers-{}", random_token(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("porchlight.json");
        let shared = Shared {
            store: store.clone(),
            auth: auth.clone(),
            registry: registry.clone(),
            client_metadata: ClientMetadata::default(),
            config: Config::at(config_path.clone()),
        };
        Gateway::new(daemon.clone(), shared.clone()).unwrap().serve(gateway_listener);
        ApprovalPages::new(daemon.clone(), Arc::new(NoReload), shared.clone()).serve(approval_listener);
        let tokens = ["paper", "split", "allowlist", "fixture", "vault", "home"]
            .into_iter()
            .map(|link| (link.to_owned(), auth.create_static_token(link, "test", 60_000).unwrap().1))
            .collect();
        let expired = auth.create_static_token("paper", "expired", -1_000).unwrap().1;

        Self {
            registry,
            auth,
            store,
            upstream,
            upstream_port,
            gateway: format!("http://127.0.0.1:{gateway_port}"),
            approval: format!("http://127.0.0.1:{approval_port}"),
            approval_port,
            config_path,
            tokens,
            expired,
            client: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
            daemon,
            shared,
        }
    }

    fn token(&self, link: &str) -> String {
        self.tokens[link].clone()
    }

    async fn rpc_with(
        &self,
        link: &str,
        token: &str,
        method: &str,
        params: Value,
        extra: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut request = self
            .client
            .post(format!("{}/{link}/mcp", self.gateway))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");

        for (name, value) in extra {
            request = request.header(*name, *value);
        }

        request
            .body(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string())
            .send()
            .await
            .unwrap()
    }

    async fn rpc(&self, link: &str, method: &str, params: Value) -> reqwest::Response {
        self.rpc_with(link, &self.token(link), method, params, &[]).await
    }

    async fn text(&self, link: &str, method: &str, params: Value) -> String {
        self.rpc(link, method, params).await.text().await.unwrap()
    }

    async fn json(&self, link: &str, method: &str, params: Value) -> Value {
        self.rpc(link, method, params).await.json().await.unwrap()
    }

    async fn events(&self, link: &str, token: &str) -> futures::stream::BoxStream<'static, String> {
        let response = self
            .client
            .get(format!("{}/{link}/mcp", self.gateway))
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();

        response.bytes_stream().map(|chunk| String::from_utf8_lossy(&chunk.unwrap_or_default()).into_owned()).boxed()
    }

    fn received(&self) -> usize {
        self.upstream.received.lock().unwrap().len()
    }
}

async fn next_within(stream: &mut futures::stream::BoxStream<'static, String>, millis: u64) -> Option<String> {
    tokio::time::timeout(Duration::from_millis(millis), stream.next()).await.ok().flatten()
}

#[tokio::test]
async fn rejects_requests_without_a_valid_bearer_token_and_points_at_resource_metadata() {
    let harness = Harness::start().await;
    let response = harness.rpc_with("paper", "nope", "tools/list", json!({}), &[]).await;

    assert_eq!(response.status(), 401);
    assert!(
        response.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("/.well-known/oauth-protected-resource/paper/mcp")
    );
    assert_eq!(harness.rpc_with("paper", &harness.expired, "tools/list", json!({}), &[]).await.status(), 401);
}

#[tokio::test]
async fn revoking_a_client_closes_its_active_event_stream() {
    let harness = Harness::start().await;
    let (_, token) = harness.auth.create_static_token("home", "stream-test", 60_000).unwrap();
    let mut stream = harness.events("home", &token).await;
    let first = next_within(&mut stream, 2_000).await;
    let grant = harness.auth.authorize(&token, "home").unwrap().unwrap();
    harness.auth.revoke_grant(&grant).unwrap();

    assert!(first.is_some());
    assert!(tokio::time::timeout(Duration::from_millis(500), stream.next()).await.unwrap().is_none());
}

#[tokio::test]
async fn filters_tools_list_in_sse_events_split_events_and_json_batches() {
    let harness = Harness::start().await;
    let batch = harness
        .client
        .post(format!("{}/paper/mcp", harness.gateway))
        .header("authorization", format!("Bearer {}", harness.token("paper")))
        .header("content-type", "application/json")
        .body(json!([{ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }]).to_string())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let bodies = [
        harness.text("paper", "tools/list", json!({})).await,
        harness.text("split", "tools/list", json!({})).await,
        batch,
    ];

    for body in bodies {
        assert!(body.contains("get_basic_info"));
        assert!(body.contains("write_html"));
        assert!(!body.contains("delete_nodes"));
    }
}

#[tokio::test]
async fn turning_a_tool_off_hides_and_refuses_it_right_away() {
    let harness = Harness::start().await;
    harness.registry.set_rules(shared_rules(&[("paper/write_html", false)])).await;
    let listed = harness.text("paper", "tools/list", json!({})).await;
    let called = harness.json("paper", "tools/call", json!({ "name": "write_html" })).await;
    harness.registry.set_rules(shared_rules(&[])).await;

    assert!(listed.contains("get_basic_info"));
    assert!(!listed.contains("write_html"));
    assert_eq!(called["error"]["code"], -32602);
    assert!(harness.text("paper", "tools/list", json!({})).await.contains("write_html"));
}

#[tokio::test]
async fn a_resource_only_change_sends_the_resource_list_notification() {
    let harness = Harness::start().await;
    let mut stream = harness.events("vault", &harness.token("vault")).await;
    next_within(&mut stream, 2_000).await;
    harness.registry.set_rules(shared_rules(&[("vault/secret", false)])).await;
    let mut heard = String::new();

    while let Some(chunk) = next_within(&mut stream, 500).await {
        heard.push_str(&chunk);
    }

    assert!(heard.contains("notifications/resources/list_changed"));
}

#[tokio::test]
async fn http_tool_list_changes_refresh_the_cached_list_and_notify_connected_clients() {
    let harness = Harness::start().await;
    *harness.upstream.dynamic.lock().unwrap() = vec![json!({ "name": "before_refresh" })];
    let url = format!("http://127.0.0.1:{}/dynamic/mcp", harness.upstream_port);
    harness.registry.add(http_spec("dynamic", &url, false)).await;
    let specs: Vec<AppSpec> = harness.registry.list().into_iter().map(|entry| entry.spec).collect();
    let mut links = base_links();
    links.push(direct("dynamic"));
    harness.registry.sync(specs, links).await;
    reaches(&harness.registry, "dynamic", ServerState::Live).await;
    let (_, token) = harness.auth.create_static_token("dynamic", "refresh-test", 60_000).unwrap();
    let mut stream = harness.events("dynamic", &token).await;
    next_within(&mut stream, 2_000).await;
    harness.upstream.dynamic.lock().unwrap().push(json!({ "name": "after_refresh" }));
    let notice = next_within(&mut stream, 9_000).await.unwrap();
    let listed = harness.rpc_with("dynamic", &token, "tools/list", json!({}), &[]).await.text().await.unwrap();

    assert!(notice.contains("notifications/tools/list_changed"));
    assert!(listed.contains("after_refresh"));
}

#[tokio::test]
async fn an_upstream_call_failure_is_recorded_before_the_gateway_returns_502() {
    let harness = Harness::start().await;
    let (stop, port) = stoppable_upstream().await;
    harness.registry.add(http_spec("offline", &format!("http://127.0.0.1:{port}/mcp"), false)).await;
    let specs: Vec<AppSpec> = harness.registry.list().into_iter().map(|entry| entry.spec).collect();
    let mut links = base_links();
    links.push(direct("offline"));
    harness.registry.sync(specs, links).await;
    reaches(&harness.registry, "offline", ServerState::Live).await;
    let (_, token) = harness.auth.create_static_token("offline", "failure-test", 60_000).unwrap();
    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = harness.rpc_with("offline", &token, "tools/call", json!({ "name": "write_html" }), &[]).await;
    let audit = harness.store.audit_list(100).unwrap();

    assert_eq!(response.status(), 502);
    assert!(audit.iter().any(|entry| {
        entry.event == "tool.called"
            && entry.app.as_deref() == Some("offline")
            && entry.outcome == Some(porchlight::store::Outcome::Error)
    }));
}

#[tokio::test]
async fn an_app_that_is_off_by_default_shares_only_the_tools_turned_on_and_no_other_methods() {
    let harness = Harness::start().await;
    let listed = harness.text("allowlist", "tools/list", json!({})).await;
    let resources = harness.json("allowlist", "resources/list", json!({})).await;

    assert!(listed.contains("write_html"));
    assert!(!listed.contains("get_basic_info"));
    assert_eq!(resources["error"]["code"], -32601);
}

#[tokio::test]
async fn rejects_request_bodies_over_the_size_limit() {
    let harness = Harness::start().await;
    let response = harness
        .client
        .post(format!("{}/paper/mcp", harness.gateway))
        .header("authorization", format!("Bearer {}", harness.token("paper")))
        .header("content-type", "application/json")
        .body("x".repeat(5 * 1024 * 1024))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 413);
}

#[tokio::test]
async fn does_not_follow_redirects_from_an_upstream() {
    let harness = Harness::start().await;
    let response = harness
        .rpc("paper", "tools/call", json!({ "name": "write_html", "arguments": { "html": "redirect_me" } }))
        .await;

    assert_eq!(response.status(), 502);
    assert!(response.headers().get("location").is_none());
    assert_eq!(harness.upstream.landed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_gateway_never_serves_the_approval_page_or_the_local_page() {
    let harness = Harness::start().await;
    let status = |path: &'static str, post: bool| {
        let url = format!("{}{path}", harness.gateway);
        let request = if post { harness.client.post(url) } else { harness.client.get(url) };
        async move { request.send().await.unwrap().status() }
    };

    assert_eq!(status("/approve?req=x", false).await, 404);
    assert_eq!(status("/", false).await, 404);
    assert_eq!(status("/toggle", true).await, 404);
}

#[tokio::test]
async fn blocks_calls_to_hidden_or_unknown_tools_and_unknown_methods_before_they_reach_upstream() {
    let harness = Harness::start().await;
    let before = harness.received();

    for name in ["delete_nodes", "secret_tool"] {
        assert_eq!(harness.json("paper", "tools/call", json!({ "name": name })).await["error"]["code"], -32602);
    }

    assert_eq!(harness.json("paper", "admin/shutdown", json!({})).await["error"]["code"], -32601);
    assert_eq!(harness.received(), before);
}

#[tokio::test]
async fn forwards_allowed_calls_without_credentials_or_unknown_headers() {
    let harness = Harness::start().await;
    let extra = [("cookie", "__Host-pl_x=nonce"), ("x-forwarded-for", "10.0.0.1"), ("x-internal-admin", "yes")];
    let response =
        harness.rpc_with("paper", &harness.token("paper"), "tools/call", json!({ "name": "write_html" }), &extra).await;
    let forwarded = harness.upstream.received.lock().unwrap().last().cloned().unwrap();

    assert_eq!(response.status(), 200);

    for name in ["authorization", "cookie", "x-forwarded-for", "x-internal-admin"] {
        assert!(forwarded.get(name).is_none(), "{name} was forwarded");
    }
}

const REDIRECT_URI: &str = "https://claude.ai/callback";

const VERIFIER: &str = "a-verifier-that-only-this-client-knows";

const RESOURCE: &str = "https://tunnel.example/paper/mcp";

impl Harness {
    async fn start_connecting(&self) -> (String, String) {
        let registration: Value = self
            .client
            .post(format!("{}/oauth/register", self.gateway))
            .header("content-type", "application/json")
            .body(json!({ "redirect_uris": [REDIRECT_URI] }).to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let client_id = registration["client_id"].as_str().unwrap().to_owned();
        let challenge = pkce_challenge(VERIFIER);
        let query = [
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("resource", RESOURCE),
            ("state", "opaque"),
        ];
        let page = self.client.get(format!("{}/oauth/authorize", self.gateway)).query(&query).send().await.unwrap();
        let cookie = page.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
        let request_id = cookie.strip_prefix("__Host-pl_").unwrap().split('=').next().unwrap().to_owned();

        (cookie, request_id)
    }

    fn complete_link(&self, request_id: &str, ticket: &str) -> String {
        format!("{}/oauth/complete?req={request_id}&g={ticket}", self.gateway)
    }

    async fn open_link(&self, link: &str, cookie: Option<&str>) -> reqwest::Response {
        let mut request = self.client.get(link);

        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }

        request.send().await.unwrap()
    }

    async fn token_request(&self, fields: &[(&str, &str)]) -> (u16, Value) {
        let response = self.client.post(format!("{}/oauth/token", self.gateway)).form(fields).send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }

    async fn connect(&self) -> (url::Url, String, String) {
        let (cookie, request_id) = self.start_connecting().await;
        let ticket = self.auth.issue_ticket(&request_id);
        let redirect = self.open_link(&self.complete_link(&request_id, &ticket), Some(&cookie)).await;
        let location = url::Url::parse(redirect.headers()["location"].to_str().unwrap()).unwrap();
        let code = location.query_pairs().find(|(name, _)| name == "code").unwrap().1.into_owned();
        let (_, body) = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ])
            .await;

        (
            location,
            body["access_token"].as_str().unwrap().to_owned(),
            body["refresh_token"].as_str().unwrap().to_owned(),
        )
    }
}

#[tokio::test]
async fn a_connected_client_can_use_only_the_server_it_was_approved_for() {
    let harness = Harness::start().await;
    let (redirect, access, _) = harness.connect().await;

    assert_eq!(format!("{}://{}{}", redirect.scheme(), redirect.host_str().unwrap(), redirect.path()), REDIRECT_URI);
    assert!(redirect.query_pairs().any(|(name, value)| name == "state" && value == "opaque"));
    assert_eq!(harness.rpc_with("paper", &access, "tools/list", json!({}), &[]).await.status(), 200);
    assert_eq!(harness.rpc_with("fixture", &access, "tools/list", json!({}), &[]).await.status(), 401);
}

#[tokio::test]
async fn an_approval_link_fails_without_the_requesters_cookie_and_is_burned() {
    let harness = Harness::start().await;
    let (cookie, request_id) = harness.start_connecting().await;
    let link = harness.complete_link(&request_id, &harness.auth.issue_ticket(&request_id));

    assert_eq!(harness.open_link(&link, None).await.status(), 403);
    assert_eq!(harness.open_link(&link, Some(&cookie)).await.status(), 403);
}

#[tokio::test]
async fn a_code_cant_be_exchanged_without_the_pkce_verifier() {
    let harness = Harness::start().await;
    let (cookie, request_id) = harness.start_connecting().await;
    let link = harness.complete_link(&request_id, &harness.auth.issue_ticket(&request_id));
    let redirect = harness.open_link(&link, Some(&cookie)).await;
    let location = url::Url::parse(redirect.headers()["location"].to_str().unwrap()).unwrap();
    let code = location.query_pairs().find(|(name, _)| name == "code").unwrap().1.into_owned();
    let stolen = harness
        .token_request(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", "guess"),
            ("resource", RESOURCE),
        ])
        .await;

    assert_eq!(stolen, (400, json!({ "error": "invalid_grant" })));
}

#[tokio::test]
async fn the_approval_page_lists_the_tools_that_will_be_shared_and_no_hidden_ones() {
    let harness = Harness::start().await;
    let (_, request_id) = harness.start_connecting().await;
    let page = harness
        .client
        .get(format!("{}/approve?req={request_id}", harness.approval))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(page.contains("<code>get_basic_info</code>"));
    assert!(page.contains("<code>write_html</code>"));
    assert!(!page.contains("delete_nodes"));
}

fn csrf_of(page: &str) -> String {
    page.split("name=\"csrf\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned()
}

#[tokio::test]
async fn denying_on_the_approval_page_sends_the_client_back_with_access_denied_and_ends_the_request() {
    let harness = Harness::start().await;
    let (cookie, request_id) = harness.start_connecting().await;
    let page = harness
        .client
        .get(format!("{}/approve?req={request_id}", harness.approval))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let denied = harness
        .client
        .post(format!("{}/approve", harness.approval))
        .header("origin", &harness.approval)
        .form(&[("req", request_id.as_str()), ("csrf", &csrf_of(&page)), ("decision", "deny")])
        .send()
        .await
        .unwrap();
    let location = url::Url::parse(denied.headers()["location"].to_str().unwrap()).unwrap();
    let link = harness.complete_link(&request_id, &harness.auth.issue_ticket(&request_id));

    assert!(location.as_str().starts_with(REDIRECT_URI));
    assert!(location.query_pairs().any(|(name, value)| name == "error" && value == "access_denied"));
    assert_eq!(harness.open_link(&link, Some(&cookie)).await.status(), 403);
}

#[tokio::test]
async fn reusing_a_refresh_token_revokes_the_grant() {
    let harness = Harness::start().await;
    let (_, _, refresh) = harness.connect().await;
    let rotated = harness.token_request(&[("grant_type", "refresh_token"), ("refresh_token", &refresh)]).await;
    let reused = harness.token_request(&[("grant_type", "refresh_token"), ("refresh_token", &refresh)]).await;
    let access = rotated.1["access_token"].as_str().unwrap().to_owned();

    assert_eq!(rotated.0, 200);
    assert_eq!(reused, (400, json!({ "error": "invalid_grant" })));
    assert_eq!(harness.rpc_with("paper", &access, "tools/list", json!({}), &[]).await.status(), 401);
}

#[tokio::test]
async fn refuses_a_server_that_exposes_a_command_tool_unless_dangerous_tools_are_allowed() {
    let harness = Harness::start().await;
    let url = format!("http://127.0.0.1:{}/shell/mcp", harness.upstream_port);
    harness.registry.add(http_spec("shell", &url, false)).await;
    let refused = harness.registry.settled("shell").await.unwrap().state;
    harness.registry.add(http_spec("shell", &url, true)).await;
    let allowed = harness.registry.settled("shell").await.unwrap().state;

    assert_eq!((refused, allowed), (ServerState::Refused, ServerState::Live));
}

#[tokio::test]
async fn shares_command_apps_as_app_tool_and_marks_tools_that_change_things() {
    let harness = Harness::start().await;
    let listed = harness.json("home", "tools/list", json!({})).await;
    let names: Vec<&str> =
        listed["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    let say = listed["result"]["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == "shelf_say").unwrap();
    let root = harness
        .client
        .post(format!("{}/mcp", harness.gateway))
        .header("authorization", format!("Bearer {}", harness.token("home")))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert!(names.contains(&"shelf_say") && names.contains(&"shelf_light"));
    assert_eq!(say["annotations"]["destructiveHint"], true);
    assert_eq!(root.status(), 404);
    assert_eq!(harness.rpc_with("shelf", &harness.token("home"), "tools/list", json!({}), &[]).await.status(), 404);
    assert_eq!(
        harness
            .client
            .get(format!("{}/.well-known/oauth-protected-resource/home/mcp", harness.gateway))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn tells_a_connected_client_when_a_tool_is_turned_off_and_not_when_an_unrelated_app_changes() {
    let harness = Harness::start().await;
    let initialized = harness.json("home", "initialize", json!({})).await;
    let mut stream = harness.events("home", &harness.token("home")).await;
    next_within(&mut stream, 2_000).await;
    harness.registry.add(command_spec("outside", &json!({ "ping": "echo ping" }))).await;
    harness.registry.settled("outside").await;
    harness.registry.set_rules(shared_rules(&[("paper/write_html", false)])).await;
    let unrelated = next_within(&mut stream, 300).await;
    harness.registry.set_rules(shared_rules(&[("shelf/say", false)])).await;
    let notice = next_within(&mut stream, 2_000).await.unwrap();

    assert_eq!(initialized["result"]["capabilities"]["tools"]["listChanged"], true);
    assert!(unrelated.is_none());
    assert!(notice.contains("notifications/tools/list_changed"));
}

#[tokio::test]
async fn a_token_for_one_link_cant_reach_another() {
    let harness = Harness::start().await;

    assert_eq!(harness.rpc_with("paper", &harness.token("home"), "tools/list", json!({}), &[]).await.status(), 401);
    assert_eq!(harness.rpc_with("home", &harness.token("paper"), "tools/list", json!({}), &[]).await.status(), 401);
}

#[tokio::test]
async fn records_a_composed_tool_call_against_its_physical_app_and_tool() {
    let harness = Harness::start().await;
    harness.text("home", "tools/call", json!({ "name": "shelf_say", "arguments": { "words": "origin-check" } })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let audit = harness.store.audit_list(100).unwrap();

    assert!(audit.iter().any(|entry| {
        entry.event == "tool.called"
            && entry.app.as_deref() == Some("shelf")
            && entry.subject.as_deref() == Some("say")
            && entry.input.as_deref().is_some_and(|input| input.contains("origin-check"))
    }));
}

#[tokio::test]
async fn a_tool_that_is_off_is_hidden_and_refused_on_its_command_link() {
    let harness = Harness::start().await;
    harness.registry.set_rules(shared_rules(&[("shelf/say", false)])).await;
    let listed = harness.text("home", "tools/list", json!({})).await;
    let called =
        harness.json("home", "tools/call", json!({ "name": "shelf_say", "arguments": { "words": "hi" } })).await;

    assert!(!listed.contains("shelf_say"));
    assert!(listed.contains("shelf_light"));
    assert_eq!(called["error"]["code"], -32602);
}

#[tokio::test]
async fn an_unconfigured_app_never_joins_a_named_link() {
    let harness = Harness::start().await;
    harness.registry.add(command_spec("wild", &json!({ "allowDangerous": true, "go": "sh -c {script}" }))).await;
    harness.registry.settled("wild").await;
    let listed = harness.text("home", "tools/list", json!({})).await;

    assert!(listed.contains("shelf_say"));
    assert!(!listed.contains("wild_go"));
}

#[tokio::test]
async fn commands_shared_as_resources_are_read_on_their_link_and_are_not_tools() {
    let harness = Harness::start().await;
    let read = |uri: &'static str, link: &'static str| harness.text(link, "resources/read", json!({ "uri": uri }));

    assert!(harness.text("home", "resources/list", json!({})).await.contains("porchlight://shelf/door"));
    assert!(!harness.text("home", "tools/list", json!({})).await.contains("shelf_door"));
    assert!(read("porchlight://shelf/door", "home").await.contains("shut"));
    assert!(!read("porchlight://vault/secret", "home").await.contains("s3cret"));
    assert!(read("porchlight://vault/secret", "vault").await.contains("s3cret"));

    harness.registry.set_rules(shared_rules(&[("shelf/door", false)])).await;
    assert!(!harness.text("home", "resources/list", json!({})).await.contains("porchlight://shelf/door"));
    assert!(!read("porchlight://shelf/door", "home").await.contains("shut"));

    harness.registry.set_rules(shared_rules(&[("vault/*", false), ("vault/secret", true)])).await;
    assert!(harness.text("vault", "resources/list", json!({})).await.contains("porchlight://vault/secret"));
    assert!(read("porchlight://vault/secret", "vault").await.contains("s3cret"));
}

#[tokio::test]
async fn command_line_tools_run_without_a_shell_and_refuse_option_like_inputs() {
    let harness = Harness::start().await;
    let call = |name: &'static str, arguments: Value| {
        harness.json("home", "tools/call", json!({ "name": name, "arguments": arguments }))
    };
    let said = call("shelf_say", json!({ "words": "a; echo pwned $(id)" })).await;

    assert_eq!(said["result"]["content"][0]["text"], "said a; echo pwned $(id)");
    assert_eq!(said["result"]["isError"], false);

    for rejected in [
        call("shelf_say", json!({ "words": "--version" })).await,
        call("shelf_light", json!({ "room": "garage" })).await,
    ] {
        assert_eq!(rejected["error"]["code"], -32602);
        assert!(rejected["error"]["message"].as_str().unwrap().contains("Invalid arguments"));
    }

    harness.registry.add(command_spec("risky", &json!({ "go": "sh -c {script}" }))).await;
    assert_eq!(harness.registry.settled("risky").await.unwrap().state, ServerState::Refused);
}

fn pid_of(text: &str) -> i32 {
    text.split("pid ").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap()
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

impl Harness {
    async fn fixture_session(&self, token: &str) -> String {
        let initialized = self
            .rpc_with("fixture", token, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }), &[])
            .await;
        initialized.headers()["mcp-session-id"].to_str().unwrap().to_owned()
    }

    async fn end_session(&self, session: &str) -> u16 {
        self.client
            .delete(format!("{}/fixture/mcp", self.gateway))
            .header("authorization", format!("Bearer {}", self.token("fixture")))
            .header("mcp-session-id", session)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

#[tokio::test]
async fn stdio_servers_run_per_session_apply_the_policy_and_stop_when_the_session_ends() {
    let harness = Harness::start().await;
    let token = harness.token("fixture");
    let session = harness.fixture_session(&token).await;
    let with = [("mcp-session-id", session.as_str())];
    let listed = harness.rpc_with("fixture", &token, "tools/list", json!({}), &with).await.text().await.unwrap();
    let hidden: Value =
        harness.rpc_with("fixture", &token, "tools/call", json!({ "name": "wipe" }), &with).await.json().await.unwrap();
    let called =
        harness.rpc_with("fixture", &token, "tools/call", json!({ "name": "echo" }), &with).await.text().await.unwrap();
    let pid = pid_of(&called);

    assert!(listed.contains("echo") && !listed.contains("wipe"));
    assert_eq!(hidden["error"]["code"], -32602);
    assert_eq!(harness.end_session(&session).await, 204);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!alive(pid));
}

#[tokio::test]
async fn a_session_only_works_with_a_token_from_the_grant_that_opened_it() {
    let harness = Harness::start().await;
    let (_, other) = harness.auth.create_static_token("fixture", "other", 60_000).unwrap();
    let owner = harness.token("fixture");
    let session = harness.fixture_session(&owner).await;
    let harness = &harness;
    let status = |token: String, id: String| async move {
        harness.rpc_with("fixture", &token, "tools/list", json!({}), &[("mcp-session-id", &id)]).await.status().as_u16()
    };
    let unsealed = session.rsplit_once('.').unwrap().0.to_owned();

    assert_eq!(status(other, session.clone()).await, 404);
    assert_eq!(status(owner.clone(), format!("{session}x")).await, 404);
    assert_eq!(status(owner.clone(), unsealed).await, 404);
    assert_eq!(status(owner, session.clone()).await, 200);
    harness.end_session(&session).await;
}

#[tokio::test]
async fn a_stdio_session_comes_back_under_the_same_id_after_its_process_is_gone_until_it_is_ended() {
    let harness = Harness::start().await;
    let token = harness.token("fixture");
    let session = harness.fixture_session(&token).await;
    let call = |id: i64| {
        let token = token.clone();
        let session = session.clone();
        let harness = &harness;
        async move {
            harness
                .client
                .post(format!("{}/fixture/mcp", harness.gateway))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .header("mcp-session-id", session)
                .body(
                    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "echo" } })
                        .to_string(),
                )
                .send()
                .await
                .unwrap()
        }
    };
    let before = pid_of(&call(1).await.text().await.unwrap());
    harness.registry.add(fixture_spec()).await;
    reaches(&harness.registry, "fixture", ServerState::Live).await;
    let (after, alongside) = tokio::join!(call(1), call(2));
    let after = pid_of(&after.text().await.unwrap());
    let alongside = pid_of(&alongside.text().await.unwrap());
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(alongside, after);
    assert_ne!(after, before);
    assert!(!alive(before));
    assert_eq!(harness.end_session(&session).await, 204);
    assert_eq!(call(3).await.status(), 404);
}

#[tokio::test]
async fn a_session_opened_before_a_gateway_restart_keeps_working_with_the_same_grant() {
    let harness = Harness::start().await;
    let initialized = harness.rpc("paper", "initialize", json!({})).await;
    let session = initialized.headers()["mcp-session-id"].to_str().unwrap().to_owned();
    let listener = bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    Gateway::new(harness.daemon.clone(), harness.shared.clone()).unwrap().serve(listener);
    let response = harness
        .client
        .post(format!("http://127.0.0.1:{port}/paper/mcp"))
        .header("authorization", format!("Bearer {}", harness.token("paper")))
        .header("content-type", "application/json")
        .header("mcp-session-id", &session)
        .body(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }).to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_approval_listener_serves_only_its_exact_loopback_host() {
    let harness = Harness::start().await;
    let status = |host: String| {
        let request = harness.client.get(format!("{}/approve?req=x", harness.approval)).header("host", host);
        async move { request.send().await.unwrap().status().as_u16() }
    };
    let port = harness.approval_port;

    assert_ne!(status(format!("127.0.0.1:{port}")).await, 403);

    for host in [format!("localhost:{port}"), "tunnel.example".to_owned(), format!("127.0.0.1:{}", port + 1)] {
        assert_eq!(status(host).await, 403);
    }
}

#[tokio::test]
async fn the_approval_listener_rejects_a_cross_origin_post() {
    let harness = Harness::start().await;
    let response = harness
        .client
        .post(format!("{}/approve", harness.approval))
        .header("origin", "https://tunnel.example")
        .form(&[("req", "x"), ("csrf", "y"), ("decision", "allow")])
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 403);
    assert_eq!(response.text().await.unwrap(), "Bad origin");
}

#[tokio::test]
async fn the_local_pages_switches_refuse_other_sites_and_forms_without_its_token() {
    let harness = Harness::start().await;
    let page = harness.client.get(format!("{}/", harness.approval)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let csrf = csrf_of(&page.text().await.unwrap());
    let toggle = |origin: &'static str, fields: Vec<(&'static str, String)>| {
        let origin = if origin.is_empty() { harness.approval.clone() } else { origin.to_owned() };
        let request =
            harness.client.post(format!("{}/toggle", harness.approval)).header("origin", origin).form(&fields);
        async move { request.send().await.unwrap().status().as_u16() }
    };
    let target = || ("target", "paper/write_html".to_owned());
    let off = || ("on", "false".to_owned());

    assert_eq!(toggle("https://tunnel.example", vec![("csrf", csrf.clone()), target(), off()]).await, 403);
    assert_eq!(toggle("", vec![target(), off()]).await, 403);
    assert_eq!(toggle("", vec![("csrf", "guess".to_owned()), target(), off()]).await, 403);
    assert_eq!(toggle("", vec![("csrf", csrf), target(), off()]).await, 303);
    assert!(std::fs::read_to_string(&harness.config_path).unwrap().contains("\"paper/write_html\": false"));
}

#[tokio::test]
async fn the_local_page_can_turn_a_disabled_mcp_app_back_on() {
    let harness = Harness::start().await;
    std::fs::write(
        &harness.config_path,
        json!({ "mcp": { "disabled": { "url": "http://127.0.0.1:9/mcp", "enabled": false } } }).to_string(),
    )
    .unwrap();
    let html = harness.client.get(format!("{}/", harness.approval)).send().await.unwrap().text().await.unwrap();
    let response = harness
        .client
        .post(format!("{}/toggle", harness.approval))
        .header("origin", &harness.approval)
        .form(&[("csrf", csrf_of(&html).as_str()), ("target", "mcp:disabled"), ("on", "true")])
        .send()
        .await
        .unwrap();
    let settings = Config::at(harness.config_path.clone()).load().unwrap().settings;

    assert_eq!(response.status(), 303);
    assert!(settings.mcp["disabled"].enabled());
}
