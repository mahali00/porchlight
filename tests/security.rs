#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use indexmap::IndexMap;
use porchlight::apps::{AppSpec, Environment, apps_from};
use porchlight::auth::{Auth, AuthError, AuthorizationRequest};
use porchlight::config::{Settings, decode_command_app};
use porchlight::core::now_ms;
use porchlight::crypto::{pkce_challenge, sha256};
use porchlight::sources::commands::{CommandSource, CommandsSpec, fill, inputs_of, literal, runs_client_code};
use porchlight::sources::{SourceRequest, SourceSpec};
use porchlight::store::{Store, TokenKind};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

fn auth() -> Auth {
    Auth::new(Store::in_memory().unwrap())
}

fn refusal<T>(result: Result<T, AuthError>) -> String {
    match result {
        Err(AuthError::Refused(message)) => message.to_owned(),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("expected a refusal"),
    }
}

fn begin(auth: &Auth, request_id: &str) -> Result<String, AuthError> {
    auth.begin(
        AuthorizationRequest {
            id: request_id.to_owned(),
            client_id: "claude".to_owned(),
            server: "paper".to_owned(),
            redirect_uri: "https://claude.ai/callback".to_owned(),
            challenge: pkce_challenge("verifier"),
            state: String::new(),
        },
        "203.0.113.9",
    )
}

fn guess_then_enter(auth: &Auth, request_id: &str, wrong: usize) -> Result<String, AuthError> {
    let nonce = begin(auth, request_id)?;
    let code = auth.issue_device_code(request_id)?;

    for _ in 0..wrong {
        assert!(auth.approve_with_code(request_id, "WRONG123", Some(&nonce)).is_err());
    }

    auth.approve_with_code(request_id, &code, Some(&nonce)).map(|approval| approval.redirect_uri)
}

#[test]
fn a_device_code_still_works_after_four_wrong_attempts() {
    assert_eq!(guess_then_enter(&auth(), "typo", 4).unwrap(), "https://claude.ai/callback");
}

#[test]
fn a_device_code_locks_after_five_wrong_attempts() {
    assert_eq!(refusal(guess_then_enter(&auth(), "guess", 5)), "Too many attempts");
}

#[test]
fn removing_a_server_invalidates_its_approved_authorization_codes() {
    let auth = auth();
    let nonce = begin(&auth, "remove-before-exchange").unwrap();
    let ticket = auth.issue_ticket("remove-before-exchange");
    let approval = auth.approve_with_ticket("remove-before-exchange", &ticket, Some(&nonce)).unwrap();
    auth.revoke_server("paper").unwrap();

    assert_eq!(refusal(auth.exchange_code(&approval.code, "verifier", "")), "invalid_grant");
}

#[test]
fn revoking_a_grant_aborts_streams_using_that_grant() {
    let auth = auth();
    let (_, token) = auth.create_static_token("paper", "client", 60_000).unwrap();
    let grant = auth.authorize(&token, "paper").unwrap().unwrap();
    let stream = auth.open_grant_stream(&grant);
    auth.revoke_grant(&grant).unwrap();

    assert!(stream.token.is_cancelled());
}

#[test]
fn revoking_an_access_token_also_ends_its_refresh_token_and_streams() {
    let auth = auth();
    let nonce = begin(&auth, "revoke-family").unwrap();
    let ticket = auth.issue_ticket("revoke-family");
    let approval = auth.approve_with_ticket("revoke-family", &ticket, Some(&nonce)).unwrap();
    let pair = auth.exchange_code(&approval.code, "verifier", "").unwrap();
    let grant = auth.authorize(&pair.access, "paper").unwrap().unwrap();
    let stream = auth.open_grant_stream(&grant);
    auth.revoke(&pair.access).unwrap();

    assert_eq!(auth.authorize(&pair.access, "paper").unwrap(), None);
    assert_eq!(refusal(auth.refresh(&pair.refresh)), "Refresh token reused; grant revoked");
    assert!(stream.token.is_cancelled());
}

#[test]
fn pending_connection_requests_are_capped() {
    let auth = auth();

    for index in 1..=20 {
        begin(&auth, &format!("flood-{index}")).unwrap();
    }

    assert_eq!(refusal(begin(&auth, "one-too-many")), "Too many pending requests");
}

#[test]
fn an_expired_refresh_token_is_refused_without_revoking_the_grant() {
    let auth = auth();
    let (_, token) = auth.create_static_token("paper", "client", 60_000).unwrap();
    let grant = auth.authorize(&token, "paper").unwrap().unwrap();
    auth.store().token_insert(&sha256("expired"), TokenKind::Refresh, &grant, "paper", now_ms() - 1_000).unwrap();

    assert_eq!(refusal(auth.refresh("expired")), "Refresh token expired");
    assert_eq!(auth.authorize(&token, "paper").unwrap(), Some(grant));
}

#[test]
fn pruning_keeps_used_refresh_tokens_so_a_later_reuse_still_revokes_the_grant() {
    let auth = auth();
    let (_, token) = auth.create_static_token("paper", "client", 60_000).unwrap();
    let grant = auth.authorize(&token, "paper").unwrap().unwrap();
    auth.store().token_insert(&sha256("used"), TokenKind::Refresh, &grant, "paper", now_ms() + 60_000).unwrap();
    auth.refresh("used").unwrap();
    auth.store().tokens_prune(now_ms()).unwrap();

    assert_eq!(refusal(auth.refresh("used")), "Refresh token reused; grant revoked");
    assert_eq!(auth.authorize(&token, "paper").unwrap(), None);
}

fn environment(variables: &[(&str, &str)], secret: Option<&'static str>) -> Environment {
    Environment {
        variables: variables
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>(),
        path: std::env::var("PATH").unwrap_or_default(),
        read_file: Box::new(move |path: &Path| secret.filter(|_| path.ends_with("secret")).map(str::to_owned)),
    }
}

fn settings(commands: &Value) -> Settings {
    Settings {
        commands: commands
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, app)| (name.clone(), decode_command_app(app).unwrap()))
            .collect(),
        ..Settings::default()
    }
}

fn compile(commands: &Value) -> porchlight::apps::Apps {
    apps_from(&settings(commands), &environment(&[], None), None)
}

fn tools_of(spec: &AppSpec) -> Vec<String> {
    spec.source.contribution().map(|contribution| contribution.tools.keys().cloned().collect()).unwrap_or_default()
}

#[test]
fn a_value_read_from_the_environment_or_a_file_never_becomes_client_input() {
    let compiled = apps_from(
        &settings(&json!({ "leak": "echo {env:SNEAKY} {file:~/secret} {words}" })),
        &environment(&[("SNEAKY", "{cmd}")], Some("{other}\n")),
        None,
    );
    let tool = &compiled.specs[0].source.contribution().unwrap().tools["leak"];
    let values = json!({ "words": "hi" });

    assert!(compiled.problems.is_empty());
    assert_eq!(inputs_of(&tool.words), ["words"]);
    assert_eq!(fill(&tool.words, values.as_object().unwrap(), &IndexMap::new())[1..], ["{cmd}", "{other}", "hi"]);
}

#[test]
fn app_names_cant_contain_underscores_so_composed_names_cant_collide() {
    let compiled = compile(&json!({ "a_b": { "c": "echo one" }, "a": { "b_c": "echo two" } }));

    assert_eq!(compiled.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(), ["a"]);
    assert_eq!(compiled.problems.iter().map(|problem| problem.app.as_str()).collect::<Vec<_>>(), ["a_b"]);
}

#[test]
fn a_composed_tool_name_longer_than_64_characters_is_refused() {
    let app = "a".repeat(40);
    let long = format!("t{}", "x".repeat(30));
    let compiled = compile(&json!({ app.clone(): { long: "echo long", "short": "echo ok" } }));

    assert_eq!(tools_of(&compiled.specs[0]), ["short"]);
    assert_eq!(compiled.problems.iter().map(|problem| problem.app.clone()).collect::<Vec<_>>(), [app]);
}

#[test]
fn named_links_overlap_and_the_wildcard_selects_every_command_app() {
    let mut settings = settings(&json!({ "lamp": "echo lamp", "notes": "echo notes" }));
    let links: Value = json!({ "home": ["lamp"], "work": ["lamp", "notes"], "everything": "*" });
    settings.links = links
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, members)| {
            let members = match members {
                Value::String(all) => porchlight::config::LinkMembers::All(all.clone()),
                other => porchlight::config::LinkMembers::Apps(serde_json::from_value(other.clone()).unwrap()),
            };
            (name.clone(), members)
        })
        .collect();
    let compiled = apps_from(&settings, &environment(&[], None), None);
    let plans: Vec<(String, String)> =
        compiled.links.iter().map(|plan| (plan.name().to_owned(), plan.signature())).collect();

    assert!(compiled.problems.is_empty());
    assert_eq!(
        plans,
        [
            ("home".to_owned(), "commands:lamp".to_owned()),
            ("work".to_owned(), "commands:lamp,notes".to_owned()),
            ("everything".to_owned(), "commands:lamp,notes".to_owned()),
        ]
    );
}

async fn call_command(app: Value, arguments: Value) -> String {
    let compiled = compile(&json!({ "t": app }));
    let Some(SourceSpec::Commands { contribution, .. }) = compiled.specs.first().map(|spec| &spec.source) else {
        panic!("didn't compile: {:?}", compiled.problems);
    };
    let source = CommandSource::new(CommandsSpec {
        name: "t".to_owned(),
        tools: contribution.tools.clone(),
        uri_of: Box::new(str::to_owned),
        instructions: None,
        notices: None,
    });
    let body =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "t", "arguments": arguments } });
    let response = source
        .handle(SourceRequest {
            method: http::Method::POST,
            headers: http::HeaderMap::new(),
            body: body.to_string().into(),
        })
        .await;
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let reply: Value = serde_json::from_slice(&bytes).unwrap();

    match reply.get("error") {
        Some(error) => format!("refused: {}", error["message"]),
        None => reply["result"]["content"][0]["text"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test]
async fn a_placeholder_inside_a_word_cant_add_an_argument() {
    let app = json!({ "run": "printf \"[%s]\" --seconds={seconds}" });

    assert_eq!(call_command(app, json!({ "seconds": "5 --rm" })).await, "[--seconds=5 --rm]");
}

#[tokio::test]
async fn typed_inputs_refuse_a_whole_word_negative_number_and_values_outside_their_limits() {
    let count =
        json!({ "run": "printf \"[%s]\" {count}", "inputs": { "count": { "type": "integer", "min": 1, "max": 10 } } });
    let signed = json!({ "run": "printf \"[%s]\" {offset}", "inputs": { "offset": { "type": "number" } } });
    let choice = json!({ "run": "printf \"[%s]\" {room}", "inputs": { "room": { "choices": ["kitchen"] } } });

    assert_eq!(call_command(count.clone(), json!({ "count": 5 })).await, "[5]");
    assert!(call_command(count.clone(), json!({ "count": 11 })).await.starts_with("refused"));
    assert!(call_command(count, json!({ "count": 2.5 })).await.starts_with("refused"));
    assert!(call_command(signed, json!({ "offset": -5 })).await.starts_with("refused"));
    assert!(call_command(choice, json!({ "room": "garage" })).await.starts_with("refused"));
}

#[tokio::test]
async fn a_boolean_input_only_ever_adds_its_fixed_flag() {
    let app = json!({ "run": "printf \"[%s]\" {verbose} x", "inputs": { "verbose": { "type": "boolean", "flag": "--verbose" } } });

    assert_eq!(call_command(app.clone(), json!({ "verbose": true })).await, "[--verbose][x]");
    assert_eq!(call_command(app.clone(), json!({ "verbose": false })).await, "[x]");
    assert!(call_command(app, json!({ "verbose": "--rm" })).await.starts_with("refused"));
}

#[tokio::test]
async fn an_optional_input_that_is_left_out_removes_its_word() {
    let app = json!({ "run": "printf \"[%s]\" {name} x", "inputs": { "name": { "optional": true } } });

    assert_eq!(call_command(app.clone(), json!({})).await, "[x]");
    assert_eq!(call_command(app, json!({ "name": "y" })).await, "[y][x]");
}

#[tokio::test]
async fn an_input_that_starts_a_word_cant_make_that_word_an_option() {
    let sized = json!({ "run": "printf \"[%s]\" {n}px", "inputs": { "n": { "type": "integer" } } });

    assert!(
        call_command(json!({ "run": "printf \"[%s]\" {name}.txt" }), json!({ "name": "-rf" }))
            .await
            .starts_with("refused")
    );
    assert!(
        call_command(json!({ "run": "printf \"[%s]\" {a}{b}" }), json!({ "a": "", "b": "--exec=x" }))
            .await
            .starts_with("refused")
    );
    assert!(call_command(sized, json!({ "n": -5 })).await.starts_with("refused"));
    assert_eq!(
        call_command(json!({ "run": "printf \"[%s]\" {name}.txt" }), json!({ "name": "notes" })).await,
        "[notes.txt]"
    );
}

#[test]
fn awk_with_client_input_is_treated_as_code_execution() {
    let compiled = compile(&json!({ "audit": "awk {program}" }));
    let tool = &compiled.specs[0].source.contribution().unwrap().tools["audit"];

    assert!(runs_client_code(&tool.words));
}

#[test]
fn quoted_assignments_stay_one_argument_when_compiling_a_command_string() {
    let compiled = compile(&json!({ "message": "printf \"[%s]\" --message=\"hello world\"" }));
    let tool = &compiled.specs[0].source.contribution().unwrap().tools["message"];
    let words: Vec<String> = tool.words.iter().map(literal).collect();

    assert_eq!(words[1..], ["[%s]", "--message=hello world"]);
    assert!(words[0].ends_with("/printf"));
}

#[test]
fn a_non_executable_program_path_is_rejected_while_compiling() {
    let dir = std::env::temp_dir().join(format!("porchlight-command-{}", porchlight::crypto::random_token(6)));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("not-executable");
    std::fs::write(&file, "echo no").unwrap();
    let compiled = compile(&json!({ "blocked": file.display().to_string() }));

    assert!(compiled.specs.is_empty());
    assert_eq!(compiled.problems.len(), 1);
}

#[test]
fn a_failed_dangerous_sibling_keeps_the_whole_app_dangerous() {
    let compiled = compile(&json!({
        "garage": { "status": "echo private", "exec": { "run": "missing-program {code}", "allowDangerous": true } }
    }));

    assert_eq!(compiled.problems.len(), 1);
    assert!(compiled.specs[0].allow_dangerous);
}

#[test]
fn an_optional_input_cant_sit_right_after_an_option() {
    let compiled =
        compile(&json!({ "t": { "run": "printf %s -o {out} {file}", "inputs": { "out": { "optional": true } } } }));

    assert!(compiled.specs.is_empty());
    assert_eq!(compiled.problems.len(), 1);
}

#[test]
fn input_settings_that_would_silently_do_nothing_are_reported() {
    let cases = [
        json!({ "run": "printf %s {x}", "inputs": { "x": { "flag": "--x" } } }),
        json!({ "run": "printf %s --x={x}", "inputs": { "x": { "type": "boolean", "flag": "--x" } } }),
        json!({ "run": "printf %s {x}", "inputs": { "x": { "type": "integer", "choices": ["1"] } } }),
        json!({ "run": "printf %s {x}", "inputs": { "x": { "min": 1 } } }),
    ];

    for app in cases {
        assert_eq!(compile(&json!({ "t": app })).problems.len(), 1);
    }
}

#[test]
fn loopback_redirects_may_use_any_port_and_nothing_else_may_differ() {
    use porchlight::client_metadata::redirect_matches;

    assert!(redirect_matches("http://127.0.0.1/callback", "http://127.0.0.1:60410/callback"));
    assert!(redirect_matches("http://localhost:3000/callback", "http://localhost:51234/callback"));
    assert!(redirect_matches("https://claude.ai/api/mcp/auth_callback", "https://claude.ai/api/mcp/auth_callback"));

    assert!(!redirect_matches("http://127.0.0.1/callback", "http://127.0.0.1:60410/other"));
    assert!(!redirect_matches("http://127.0.0.1/callback", "http://localhost:60410/callback"));
    assert!(!redirect_matches("http://127.0.0.1/callback", "http://127.0.0.1:60410/callback?next=evil"));
    assert!(!redirect_matches("http://127.0.0.1/callback", "http://evil@127.0.0.1:60410/callback"));
    assert!(!redirect_matches("http://127.0.0.1/callback", "https://evil.example/callback"));
    assert!(!redirect_matches("https://claude.ai/cb", "https://claude.ai:8443/cb"));
    assert!(!redirect_matches("http://evil.example/cb", "http://evil.example:81/cb"));
}

#[test]
fn a_forwarded_address_is_recorded_as_a_claim_not_a_fact() {
    use http::{HeaderMap, HeaderValue};
    use porchlight::gateway::source_of;

    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    };

    assert_eq!(source_of(&headers(&[("host", "127.0.0.1:18080")])), "this computer");
    assert_eq!(source_of(&headers(&[("host", "porchlight.x.opentunnel.xyz")])), "the tunnel");
    assert_eq!(
        source_of(&headers(&[("host", "porchlight.x.opentunnel.xyz"), ("x-forwarded-for", "6.6.6.6")])),
        "the tunnel, says 6.6.6.6"
    );
}
