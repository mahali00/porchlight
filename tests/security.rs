#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use indexmap::IndexMap;
use porchlight::apps::{Environment, apps_from};
use porchlight::auth::{Auth, AuthError, AuthorizationRequest};
use porchlight::config::Input;
use porchlight::config::{Settings, decode_command_app};
use porchlight::core::now_ms;
use porchlight::crypto::{pkce_challenge, random_token, sha256};
use porchlight::policy::Level;
use porchlight::sources::AppSpec;
use porchlight::sources::commands::{
    CommandSource, CommandsSpec, assess, command_tools, fill, inputs_of, literal, runs_client_code,
};
use porchlight::sources::{SourceRequest, SourceSpec};
use porchlight::spec::template_words;
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
            challenge: pkce_challenge("verifier-012345678901234567890123456789012345"),
            state: String::new(),
        },
        "203.0.113.9",
    )
}

fn guess_then_enter(auth: &Auth, request_id: &str, wrong: usize) -> Result<String, AuthError> {
    let nonce = begin(auth, request_id)?;
    let code = auth.store().pending_get(request_id)?.map(|request| request.code).unwrap_or_default();

    for _ in 0..wrong {
        assert!(auth.approve_with_code(request_id, "WRONG123", Some(&nonce)).is_err());
    }

    auth.approve_with_code(request_id, &code, Some(&nonce)).map(|approval| approval.redirect_uri)
}

#[test]
fn a_pending_request_always_has_a_code_and_a_wrong_code_is_refused() {
    let auth = auth();
    let nonce = begin(&auth, "wrong-code").unwrap();
    let code = auth.store().pending_get("wrong-code").unwrap().unwrap().code;

    assert!(!code.is_empty());
    assert_eq!(refusal(auth.approve_with_code("wrong-code", "WRONG123", Some(&nonce))), "Invalid code");
    assert_eq!(
        auth.approve_with_code("wrong-code", &code, Some(&nonce)).unwrap().redirect_uri,
        "https://claude.ai/callback"
    );
}

#[test]
fn an_expired_pending_code_is_refused() {
    let path = std::env::temp_dir().join(format!("porchlight-expired-code-{}", random_token(8)));
    let store = Store::open(&path).unwrap();
    let auth = Auth::new(store.clone());
    let nonce = begin(&auth, "expired-code").unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE pending SET expires_at = ? WHERE id = ?", rusqlite::params![now_ms() - 1, "expired-code"])
        .unwrap();
    drop(connection);

    assert_eq!(refusal(auth.approve_with_code("expired-code", "ANYCODE", Some(&nonce))), "Code expired");

    drop(auth);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn an_approval_code_still_works_after_four_wrong_attempts() {
    assert_eq!(guess_then_enter(&auth(), "typo", 4).unwrap(), "https://claude.ai/callback");
}

#[test]
fn an_approval_code_locks_after_five_wrong_attempts() {
    assert_eq!(refusal(guess_then_enter(&auth(), "guess", 5)), "Code expired");
}

#[test]
fn removing_a_server_invalidates_its_approved_authorization_codes() {
    let auth = auth();
    let nonce = begin(&auth, "remove-before-exchange").unwrap();
    let code = auth.store().pending_get("remove-before-exchange").unwrap().unwrap().code;
    let approval = auth.approve_with_code("remove-before-exchange", &code, Some(&nonce)).unwrap();
    auth.revoke_server("paper").unwrap();

    assert_eq!(
        refusal(auth.exchange_code(
            &approval.code,
            "verifier-012345678901234567890123456789012345",
            "",
            "claude",
            "https://claude.ai/callback",
        )),
        "invalid_grant"
    );
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
    let code = auth.store().pending_get("revoke-family").unwrap().unwrap().code;
    let approval = auth.approve_with_code("revoke-family", &code, Some(&nonce)).unwrap();
    let pair = auth
        .exchange_code(
            &approval.code,
            "verifier-012345678901234567890123456789012345",
            "",
            "claude",
            "https://claude.ai/callback",
        )
        .unwrap();
    let grant = auth.authorize(&pair.access, "paper").unwrap().unwrap();
    let stream = auth.open_grant_stream(&grant);
    auth.revoke(&pair.access).unwrap();

    assert_eq!(auth.authorize(&pair.access, "paper").unwrap(), None);
    assert_eq!(refusal(auth.refresh(&pair.refresh)), "invalid_grant");
    assert!(stream.token.is_cancelled());
}

#[test]
fn two_correct_code_submissions_mint_at_most_one_authorization_code() {
    let auth = auth();
    let nonce = begin(&auth, "parallel-code").unwrap();
    let code = auth.store().pending_get("parallel-code").unwrap().unwrap().code;
    let first = auth.clone();
    let second = auth.clone();

    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| first.approve_with_code("parallel-code", &code, Some(&nonce)).is_ok());
        let second = scope.spawn(|| second.approve_with_code("parallel-code", &code, Some(&nonce)).is_ok());
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_eq!(results.into_iter().filter(|approved| *approved).count(), 1);
}

#[test]
fn refresh_token_reuse_revokes_grant_and_closes_its_streams() {
    let auth = auth();
    let nonce = begin(&auth, "refresh-reuse").unwrap();
    let code = auth.store().pending_get("refresh-reuse").unwrap().unwrap().code;
    let approval = auth.approve_with_code("refresh-reuse", &code, Some(&nonce)).unwrap();
    let pair = auth
        .exchange_code(
            &approval.code,
            "verifier-012345678901234567890123456789012345",
            "",
            "claude",
            "https://claude.ai/callback",
        )
        .unwrap();
    let grant = auth.authorize(&pair.access, "paper").unwrap().unwrap();
    let stream = auth.open_grant_stream(&grant);
    let rotated = auth.refresh(&pair.refresh).unwrap();

    assert_eq!(refusal(auth.refresh(&pair.refresh)), "Refresh token reused; grant revoked");
    assert_eq!(auth.authorize(&rotated.access, "paper").unwrap(), None);
    assert!(stream.token.is_cancelled());
}

#[test]
fn registration_limit_is_per_source_and_global() {
    let auth = auth();

    for _ in 0..5 {
        assert!(auth.allow_registration("source-a").unwrap());
    }

    assert!(!auth.allow_registration("source-a").unwrap());
    assert!(auth.allow_registration("source-b").unwrap());
}

#[test]
fn link_membership_signatures_distinguish_comma_containing_names() {
    use porchlight::links::LinkPlan;

    let combined =
        LinkPlan::Commands { name: "shared".to_owned(), apps: Vec::new(), membership: vec!["a,b".to_owned()] };
    let separate = LinkPlan::Commands {
        name: "shared".to_owned(),
        apps: Vec::new(),
        membership: vec!["a".to_owned(), "b".to_owned()],
    };

    assert_ne!(combined.signature(), separate.signature());
}

#[test]
fn authorization_codes_require_the_request_client_and_redirect_uri() {
    let auth = auth();
    let nonce = begin(&auth, "wrong-client").unwrap();
    let code = auth.store().pending_get("wrong-client").unwrap().unwrap().code;
    let approval = auth.approve_with_code("wrong-client", &code, Some(&nonce)).unwrap();

    assert_eq!(
        refusal(auth.exchange_code(
            &approval.code,
            "verifier-012345678901234567890123456789012345",
            "",
            "other-client",
            "https://claude.ai/callback",
        )),
        "invalid_grant"
    );
}

#[test]
fn authorization_code_rejects_invalid_pkce_verifier_syntax() {
    let auth = auth();
    let nonce = begin(&auth, "invalid-verifier").unwrap();
    let code = auth.store().pending_get("invalid-verifier").unwrap().unwrap().code;
    let approval = auth.approve_with_code("invalid-verifier", &code, Some(&nonce)).unwrap();

    assert_eq!(
        refusal(auth.exchange_code(&approval.code, "short", "", "claude", "https://claude.ai/callback")),
        "Invalid PKCE verifier"
    );
}

#[test]
fn authorization_codes_require_the_original_redirect_uri() {
    let auth = auth();
    let nonce = begin(&auth, "wrong-redirect").unwrap();
    let code = auth.store().pending_get("wrong-redirect").unwrap().unwrap().code;
    let approval = auth.approve_with_code("wrong-redirect", &code, Some(&nonce)).unwrap();

    assert_eq!(
        refusal(auth.exchange_code(
            &approval.code,
            "verifier-012345678901234567890123456789012345",
            "",
            "claude",
            "https://attacker.example/callback",
        )),
        "invalid_grant"
    );
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
            ("home".to_owned(), "commands:4:lamp".to_owned()),
            ("work".to_owned(), "commands:4:lamp5:notes".to_owned()),
            ("everything".to_owned(), "commands:4:lamp5:notes".to_owned()),
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
fn wrappers_and_client_chosen_applets_are_treated_as_code_execution() {
    let words = |run: &str| template_words(&run.split(' ').map(str::to_owned).collect::<Vec<_>>());

    for run in [
        "nohup sh -c {script}",
        "setsid {program}",
        "stdbuf -oL bash -c {script}",
        "busybox {applet} -c {script}",
        "busybox sh -c {script}",
        "toybox nohup sh -c {script}",
    ] {
        assert!(runs_client_code(&words(run)), "{run}");
    }

    assert!(!runs_client_code(&words("busybox ls {dir}")));
}

#[test]
fn risk_follows_from_what_a_client_controls_not_from_the_program_name() {
    let words = |run: &str| template_words(&run.split(' ').map(str::to_owned).collect::<Vec<_>>());
    let open = IndexMap::new();
    let limited: IndexMap<String, Input> =
        [("room".to_owned(), serde_json::from_value(json!({ "choices": ["kitchen"] })).unwrap())].into();
    let level =
        |run: &str, inputs: &IndexMap<String, Input>, read_only: bool| assess(&words(run), inputs, read_only).level();

    assert_eq!(level("date +%F", &open, true), Level::Reads);
    assert_eq!(level("cat {name}", &open, true), Level::Reads);
    assert_eq!(level("date", &open, false), Level::Changes);
    assert_eq!(level("lights {room}", &limited, false), Level::Changes);
    assert_eq!(level("./cleanup.sh {path}", &open, false), Level::ClientChooses);
    assert_eq!(level("curl -X DELETE https://api.example.com/{id}", &open, false), Level::ClientChooses);
    assert_eq!(level("rm -rf {path}", &open, true), Level::ClientChooses);
    assert_eq!(level("sh -c {script}", &open, false), Level::RunsCode);
}

#[test]
fn an_mcp_tool_is_only_read_only_when_nothing_it_announces_says_otherwise() {
    let level = |tool: Value| porchlight::sources::announced_risk(&serde_json::from_value(tool).unwrap()).level();
    let text = json!({ "properties": { "title": { "type": "string" } } });
    let picked = json!({ "properties": { "state": { "type": "string", "enum": ["open", "closed"] } } });

    assert_eq!(level(json!({ "name": "list", "annotations": { "readOnlyHint": true } })), Level::Reads);
    assert_eq!(
        level(json!({ "name": "purge", "annotations": { "readOnlyHint": true, "destructiveHint": true } })),
        Level::Changes
    );
    assert_eq!(level(json!({ "name": "sync" })), Level::Changes);
    assert_eq!(level(json!({ "name": "close", "inputSchema": picked })), Level::Changes);
    assert_eq!(level(json!({ "name": "create_issue", "inputSchema": text })), Level::ClientChooses);
    assert_eq!(level(json!({ "name": "run_command", "annotations": { "readOnlyHint": true } })), Level::RunsCode);
}

#[test]
fn a_known_destructive_program_contradicts_a_read_only_claim_even_behind_a_wrapper() {
    let words = |run: &str| template_words(&run.split(' ').map(str::to_owned).collect::<Vec<_>>());

    for run in [
        "rm -rf {path}",
        "timeout 5 rm {path}",
        "nice -n 5 shred {file}",
        "find {dir} -delete",
        "git push --force origin {branch}",
        "docker rm {container}",
        "mkfs.ext4 {disk}",
    ] {
        assert!(assess(&words(run), &IndexMap::new(), true).contradiction().is_some(), "{run}");
    }

    for run in ["ls {dir}", "git status", "docker ps", "find {dir} -name {pattern}"] {
        assert!(assess(&words(run), &IndexMap::new(), true).contradiction().is_none(), "{run}");
    }
}

#[test]
fn a_destructive_command_is_never_announced_as_read_only() {
    let compiled = compile(&json!({ "t": { "run": "rm -rf /tmp/box/{name}", "readOnly": true } }));
    let tools = &compiled.specs[0].source.contribution().unwrap().tools;
    let annotations = command_tools(tools)[0].annotations.clone().unwrap();

    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(true));
}

#[test]
fn a_resource_cant_be_a_destructive_command() {
    let compiled = compile(&json!({ "t": { "run": "rm -rf /tmp/box", "resource": true } }));

    assert!(compiled.specs.is_empty());
    assert!(compiled.problems.iter().any(|problem| problem.message.contains("deletes files")));
}

#[tokio::test]
async fn no_input_can_climb_out_of_a_folder() {
    let changes = json!({ "run": "printf \"[%s]\" /tmp/box/{name}" });
    let reads = json!({ "run": "printf \"[%s]\" /tmp/box/{name}", "readOnly": true });

    assert!(call_command(changes.clone(), json!({ "name": "../etc" })).await.contains("can't contain .."));
    assert!(call_command(reads, json!({ "name": "notes/../../.ssh" })).await.contains("can't contain .."));
    assert_eq!(call_command(changes, json!({ "name": "a..b" })).await, "[/tmp/box/a..b]");
}

fn add_from_a_script(args: &[&str]) -> (Option<i32>, String) {
    let home = std::env::temp_dir().join(format!("porchlight-add-{}", random_token(6)));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_porchlight"))
        .args(["tools", "add"])
        .args(args)
        .env("PORCHLIGHT_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let saved = std::fs::read_to_string(home.join("porchlight.json")).unwrap_or_default();
    std::fs::remove_dir_all(&home).ok();

    (output.status.code(), saved)
}

#[test]
fn a_script_cant_add_a_tool_that_changes_things_with_input_a_client_chooses() {
    let (code, saved) = add_from_a_script(&["stamp", "--", "touch", "{name}"]);
    assert_eq!(code, Some(5));
    assert!(!saved.contains("stamp"));

    let (code, saved) = add_from_a_script(&["say", "--read-only", "--", "echo", "{text}"]);
    assert_eq!(code, Some(0));
    assert!(saved.contains("say"));

    let (code, _) = add_from_a_script(&["wipe", "--read-only", "--", "rm", "-rf", "{path}"]);
    assert_eq!(code, Some(2));
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

#[test]
fn concurrent_first_opens_leave_a_complete_sqlite_schema() {
    let path = std::env::temp_dir().join(format!("porchlight-concurrent-schema-{}", random_token(8)));

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let path = path.clone();
            scope.spawn(move || Store::open(&path).unwrap());
        }
    });

    let store = Store::open(&path).unwrap();
    assert_eq!(store.pending_count_active(now_ms()).unwrap(), 0);
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn upgrading_the_database_preserves_access_and_history_but_ends_old_pending_requests() {
    let path = std::env::temp_dir().join(format!("porchlight-upgrade-{}", random_token(8)));
    let old_schema = include_str!("fixtures/schema-v2.sql");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch(old_schema).unwrap();
    connection
        .execute_batch(
            "INSERT INTO clients VALUES ('client', '[]', 'Existing client', 1);
        INSERT INTO grants VALUES ('grant', 'client', 'tools', 1, NULL);
        INSERT INTO tokens VALUES ('token-hash', 'static', 'grant', 'tools', 9999999999999, 0);
        INSERT INTO audit (at, event) VALUES (1, 'before-upgrade');
        INSERT INTO pending (id, client_id, server, redirect_uri, challenge, nonce_hash, created_at, expires_at)
            VALUES ('old-request', 'client', 'tools', 'https://example.com', 'challenge', 'nonce', 1, 2);",
        )
        .unwrap();
    drop(connection);
    let store = Store::open(&path).unwrap();
    assert!(store.grant_exists("grant").unwrap());
    assert!(store.token_get("token-hash").unwrap().is_some());
    assert_eq!(store.client_name("client").unwrap(), "Existing client");
    assert!(store.pending_list().unwrap().is_empty());
    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM audit WHERE event = 'before-upgrade'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 3);
    drop(connection);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn opening_a_newer_database_refuses_without_deleting_its_data() {
    let path = std::env::temp_dir().join(format!("porchlight-newer-schema-{}", random_token(8)));
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA user_version = 99; CREATE TABLE future_data (value TEXT); INSERT INTO future_data VALUES ('keep');",
        )
        .unwrap();
    drop(connection);
    assert!(Store::open(&path).is_err());
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        connection.query_row("SELECT value FROM future_data", [], |row| row.get::<_, String>(0)).unwrap(),
        "keep"
    );
    assert_eq!(connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 99);
    drop(connection);
    std::fs::remove_file(path).unwrap();
}
