#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use porchlight::auth::AuthorizationRequest;
use porchlight::config::Config;
use porchlight::control::DaemonState;
use porchlight::crypto::{pkce_challenge, random_token};
use porchlight::daemon::{Options, Running, start};
use porchlight::store::Store;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct Harness {
    running: Running,
    path: PathBuf,
}

impl Harness {
    async fn start(settings: &Value) -> Self {
        let dir = std::env::temp_dir().join(format!("porchlight-config-{}", random_token(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("porchlight.json");
        std::fs::write(&path, settings.to_string()).unwrap();
        let options =
            Options { gateway_port: 0, approval_port: 0, tunnel: "https://tunnel.example".to_owned(), socket: None };
        let running = start(options, Config::at(path.clone()), Store::in_memory().unwrap()).await.unwrap();

        Self { running, path }
    }

    fn write(&self, settings: &Value) {
        std::fs::write(&self.path, settings.to_string()).unwrap();
    }

    async fn reload(&self) -> DaemonState {
        self.running.reload().await
    }

    fn token(&self, link: &str) -> String {
        self.running.shared.auth.create_static_token(link, "test", 60_000).unwrap().1
    }

    fn allowed(&self, token: &str, link: &str) -> bool {
        self.running.shared.auth.authorize(token, link).unwrap().is_some()
    }
}

fn tools(state: &DaemonState, link: &str) -> Option<usize> {
    state.servers.iter().find(|server| server.name == link).map(|server| server.tools)
}

fn problem_apps(state: &DaemonState) -> Vec<String> {
    state.problems.iter().map(|problem| problem.app.clone()).collect()
}

#[tokio::test]
async fn an_app_that_cant_run_is_reported_and_the_other_apps_stay_live() {
    let harness = Harness::start(&json!({})).await;
    harness.write(&json!({ "commands": { "a": "echo a", "b": "no-such-program-for-porchlight" }, "links": { "a": ["a"], "b": ["b"] } }));
    let state = harness.reload().await;
    let servers: Vec<(String, String, usize)> = state
        .servers
        .iter()
        .map(|server| {
            (
                server.name.clone(),
                serde_json::to_value(server.state).unwrap().as_str().unwrap().to_owned(),
                server.tools,
            )
        })
        .collect();

    assert_eq!(servers, [("a".to_owned(), "live".to_owned(), 1), ("b".to_owned(), "live".to_owned(), 0)]);
    assert_eq!(problem_apps(&state), ["b"]);

    harness.write(&json!({ "commands": { "good": "echo ready" }, "links": { "good": ["good"] }, "mcp": { "broken": { "command": 42 } } }));
    let malformed = harness.reload().await;

    assert_eq!(tools(&malformed, "good"), Some(1));
    assert_eq!(problem_apps(&malformed), ["broken"]);
}

#[tokio::test]
async fn a_malformed_app_keeps_its_existing_link_grant() {
    let harness = Harness::start(&json!({ "mcp": { "broken": { "url": "http://127.0.0.1:9/mcp" } } })).await;
    let token = harness.token("broken");
    harness.write(&json!({ "mcp": { "broken": { "command": 42 } }, "commands": { "good": "echo good" }, "links": { "good": ["good"] } }));
    harness.reload().await;

    assert!(harness.allowed(&token, "broken"));
}

#[tokio::test]
async fn a_malformed_command_app_keeps_its_named_link_grant() {
    let harness = Harness::start(&json!({ "commands": { "lamp": "echo on" }, "links": { "home": ["lamp"] } })).await;
    let token = harness.token("home");
    harness.write(&json!({ "commands": { "lamp": 42 }, "links": { "home": ["lamp"] } }));
    harness.reload().await;

    assert!(harness.allowed(&token, "home"));
}

#[tokio::test]
async fn a_refused_command_app_is_visible_in_status_when_it_belongs_to_a_composed_link() {
    let harness = Harness::start(&json!({})).await;
    harness.write(&json!({ "commands": { "shell": { "run": "sh -c {script}", "inputs": { "script": {} } } }, "links": { "home": ["shell"] } }));
    let state = harness.reload().await;

    assert_eq!(tools(&state, "home"), Some(0));
    assert!(state.problems.iter().any(|problem| problem.app == "shell" && problem.message.contains("refused")));
}

#[tokio::test]
async fn an_editors_atomic_config_replacement_is_reset_to_owner_only_permissions() {
    let harness = Harness::start(&json!({})).await;
    let replacement = harness.path.with_extension("replacement");
    std::fs::write(&replacement, json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] } }).to_string())
        .unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::rename(&replacement, &harness.path).unwrap();
    harness.reload().await;

    assert_eq!(std::fs::metadata(&harness.path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn settings_that_dont_parse_keep_the_last_good_tool_rules_in_place() {
    let harness = Harness::start(&json!({})).await;
    harness.write(&json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] }, "tools": { "a/*": false } }));
    assert_eq!(tools(&harness.reload().await, "a"), Some(0));
    harness.write(&json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] }, "tools": { "a/*": false, "not a pattern": true } }));
    let state = harness.reload().await;

    assert_eq!(tools(&state, "a"), Some(0));
    assert!(state.problems[0].message.contains("last good settings"));

    std::fs::write(
        &harness.path,
        r#"{ "commands": { "a": "echo a" }, "links": { "a": ["a"] }, "tools": { "a/*": false "#,
    )
    .unwrap();
    assert_eq!(tools(&harness.reload().await, "a"), Some(0));
}

#[tokio::test]
async fn nothing_in_porchlight_json_can_approve_a_client() {
    let harness = Harness::start(&json!({})).await;
    harness.write(&json!({
        "commands": { "a": "echo a" },
        "links": { "a": ["a"] },
        "clients": { "claude": { "approved": true } },
        "grants": [{ "client": "claude", "app": "a" }],
        "approve": "*",
    }));
    let state = harness.reload().await;
    let messages: Vec<String> = state.problems.iter().map(|problem| problem.message.clone()).collect();

    assert_eq!(harness.running.shared.store.grants_count().unwrap(), 0);
    assert_eq!(messages, ["Unknown key \"clients\"", "Unknown key \"grants\"", "Unknown key \"approve\""]);
}

#[tokio::test]
async fn removing_an_app_revokes_its_clients_even_if_an_app_with_that_name_comes_back() {
    let harness =
        Harness::start(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "a": ["a"], "b": ["b"] } }))
            .await;
    let token = harness.token("b");
    assert!(harness.allowed(&token, "b"));
    harness.write(&json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] } }));
    harness.reload().await;
    harness.write(
        &json!({ "commands": { "a": "echo a", "b": "echo something else" }, "links": { "a": ["a"], "b": ["b"] } }),
    );
    harness.reload().await;

    assert!(!harness.allowed(&token, "b"));
}

#[tokio::test]
async fn changing_a_links_app_membership_revokes_its_existing_clients() {
    let harness =
        Harness::start(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "shared": ["a"] } })).await;
    let token = harness.token("shared");
    assert!(harness.allowed(&token, "shared"));
    harness.write(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "shared": ["b"] } }));
    harness.reload().await;

    assert!(!harness.allowed(&token, "shared"));
}

#[tokio::test]
async fn editing_an_app_keeps_clients_approved_for_the_same_link_membership() {
    let harness =
        Harness::start(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "shared": ["a"] } })).await;
    let token = harness.token("shared");
    harness.write(&json!({ "commands": { "a": "echo updated", "b": "echo b" }, "links": { "shared": ["a"] } }));
    harness.reload().await;

    assert!(harness.allowed(&token, "shared"));
}

#[tokio::test]
async fn changing_a_links_app_membership_invalidates_approved_codes_before_token_exchange() {
    let harness =
        Harness::start(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "shared": ["a"] } })).await;
    let auth = &harness.running.shared.auth;
    let nonce = auth
        .begin(
            AuthorizationRequest {
                id: "approved-before-membership-change".to_owned(),
                client_id: "test-client".to_owned(),
                server: "shared".to_owned(),
                redirect_uri: "https://client.example/callback".to_owned(),
                challenge: pkce_challenge("test-verifier"),
                state: String::new(),
            },
            "203.0.113.9",
        )
        .unwrap();
    let ticket = auth.issue_ticket("approved-before-membership-change");
    let approval = auth.approve_with_ticket("approved-before-membership-change", &ticket, Some(&nonce)).unwrap();
    harness.write(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "shared": ["b"] } }));
    harness.reload().await;

    assert!(auth.exchange_code(&approval.code, "test-verifier", "https://tunnel.example/shared/mcp").is_err());
}

#[tokio::test]
async fn an_app_that_cant_start_right_now_keeps_its_clients() {
    let harness = Harness::start(&json!({ "mcp": { "notes": { "url": "http://127.0.0.1:9/mcp" } } })).await;
    let token = harness.token("notes");
    harness.write(&json!({ "mcp": { "notes": { "url": "{env:PORCHLIGHT_TEST_NEVER_SET}" } } }));
    let state = harness.reload().await;

    assert_eq!(problem_apps(&state), ["notes"]);
    assert!(harness.allowed(&token, "notes"));
}

#[tokio::test]
async fn removing_an_app_ends_the_connection_requests_still_waiting_for_it() {
    let harness =
        Harness::start(&json!({ "commands": { "a": "echo a", "b": "echo b" }, "links": { "a": ["a"], "b": ["b"] } }))
            .await;
    harness
        .running
        .shared
        .auth
        .begin(
            AuthorizationRequest {
                id: "waiting-for-b".to_owned(),
                client_id: "c".to_owned(),
                server: "b".to_owned(),
                redirect_uri: "https://c.example/cb".to_owned(),
                challenge: "x".to_owned(),
                state: String::new(),
            },
            "203.0.113.9",
        )
        .unwrap();
    harness.write(&json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] } }));
    harness.reload().await;

    assert!(harness.running.shared.store.pending_get("waiting-for-b").unwrap().is_none());
}

#[tokio::test]
async fn an_edit_that_would_leave_porchlight_json_unreadable_is_refused_and_the_file_is_left_alone() {
    let harness = Harness::start(&json!({ "commands": { "a": "echo a" }, "links": { "a": ["a"] } })).await;
    let config = Config::at(harness.path.clone());
    let before = std::fs::read_to_string(&harness.path).unwrap();

    assert!(config.edit(&["tools", "paper/"], Some(&json!(false))).is_err());
    assert!(config.edit(&["commands", "a", "environment"], Some(&json!("ls"))).is_err());
    assert_eq!(std::fs::read_to_string(&harness.path).unwrap(), before);
}

#[tokio::test]
async fn a_membership_change_while_stopped_revokes_grants_from_the_previous_run() {
    let dir = std::env::temp_dir().join(format!("porchlight-restart-{}", random_token(6)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("porchlight.json");
    let database = dir.join("state.db");
    let write = |members: &[&str]| {
        let settings =
            json!({ "commands": { "lights": "echo on", "music": "echo play" }, "links": { "home": members } });
        std::fs::write(&path, settings.to_string()).unwrap();
    };
    let run = || async {
        let options =
            Options { gateway_port: 0, approval_port: 0, tunnel: "https://tunnel.example".to_owned(), socket: None };
        start(options, Config::at(path.clone()), Store::open(&database).unwrap()).await.unwrap()
    };
    write(&["lights"]);
    let first = run().await;
    let token = first.shared.auth.create_static_token("home", "test", 60_000).unwrap().1;
    first.shutdown().await;
    write(&["music"]);
    let second = run().await;

    assert!(second.shared.auth.authorize(&token, "home").unwrap().is_none());
    second.shutdown().await;
}

#[tokio::test]
async fn edits_keep_the_comments_and_layout_of_porchlight_json() {
    let dir = std::env::temp_dir().join(format!("porchlight-edit-{}", random_token(6)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("porchlight.json");
    std::fs::write(&path, "{\n  // my lamp\n  \"commands\": {\n    \"lamp\": \"echo on\"\n  }\n}\n").unwrap();
    let config = Config::at(path.clone());
    config.edit(&["tools", "lamp/*"], Some(&json!(false))).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();

    assert!(text.contains("// my lamp"));
    assert!(text.contains("\"lamp/*\": false"));
    assert_eq!(config.load().unwrap().settings.tools.get("lamp/*"), Some(&false));
}
