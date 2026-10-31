use crate::client_metadata::ClientIdentity;
use crate::core::now_ms;
use crate::crypto::script_hash;
use crate::policy::Level;
use crate::rpc::with_status;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use std::sync::LazyLock;

const STYLE: &str = include_str!("../assets/style.css");
const SCRIPT: &str = include_str!("../assets/code.js");

static POLICY: LazyLock<Option<HeaderValue>> = LazyLock::new(|| {
    HeaderValue::from_str(&format!(
        "default-src 'none'; style-src 'unsafe-inline'; script-src {}; frame-ancestors 'none'",
        script_hash(SCRIPT)
    ))
    .ok()
});

fn secured(mut response: Response) -> Response {
    let headers = response.headers_mut();

    headers.insert(
        "content-security-policy",
        POLICY.clone().unwrap_or_else(|| HeaderValue::from_static("default-src 'none'")),
    );

    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));

    response
}

pub fn html_response(markup: &Markup, status: StatusCode) -> Response {
    let mut response = with_status(status, Body::from(markup.0.clone()));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    secured(response)
}

pub fn redirect_to(target: &str, params: &[(&str, &str)]) -> Response {
    let location = url::Url::parse(target).map_or_else(
        |_| target.to_owned(),
        |mut url| {
            let kept: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(name, _)| !params.iter().any(|(set, value)| !value.is_empty() && set == name))
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(kept)
                .extend_pairs(params.iter().filter(|(_, value)| !value.is_empty()));

            if url.query() == Some("") {
                url.set_query(None);
            }

            url.to_string()
        },
    );
    let mut response = with_status(StatusCode::FOUND, Body::empty());

    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }

    secured(response)
}

fn document(title: &str, place: &str, refresh: Option<u8>, main: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover";
                meta name="color-scheme" content="dark";
                @if let Some(seconds) = refresh { meta http-equiv="refresh" content=(seconds); }
                title { (title) " · porchlight" }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header {
                    span class="mark" { svg width="18" height="18" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true" {
                        path d="M5 4h10M7 4v2h6V4" {}
                        rect x="6" y="6" width="8" height="9" rx="2.5" {}
                        circle cx="10" cy="10.5" r="1.6" {}
                        path d="M8.5 15v2h3v-2" {}
                    } "porchlight" }
                    @if !place.is_empty() { span class="place" { (place) } }
                }
                (main)
            }
        }
    }
}

pub struct Message {
    pub title: &'static str,
    pub detail: &'static str,
    pub next: &'static str,
}

pub fn message_response(status: StatusCode, message: &Message) -> Response {
    let main = html! {
        main {
            h1 { (message.title) }
            p { (message.detail) }
            p class="trust" { (message.next) }
        }
    };

    html_response(&document(message.title, "", None, &main), status)
}

pub struct SharedTool {
    pub name: String,
    pub level: Level,
}

pub struct RequestSummary {
    pub client: ClientIdentity,
    pub server_name: String,
    pub computer: String,
    pub tools: Vec<SharedTool>,
    pub opened_at: i64,
    pub expires_at: i64,
    pub redirect_uri: String,
}

pub enum Entry {
    Fresh,
    Wrong { tries_left: i64 },
    OtherBrowser,
}

fn elsewhere(summary: &RequestSummary) -> Option<String> {
    let redirect = url::Url::parse(&summary.redirect_uri).ok()?;
    let loopback = redirect.host().is_some_and(|host| match host {
        url::Host::Domain(host) => host == "localhost",
        url::Host::Ipv4(host) => host.is_loopback(),
        url::Host::Ipv6(host) => host.is_loopback(),
    });

    if loopback { None } else { redirect.host_str().map(str::to_owned) }
}

fn client_title(client: &ClientIdentity) -> String {
    match client.verified_host {
        Some(_) => client.name.clone(),
        None => format!("“{}”", client.name),
    }
}

fn identity(client: &ClientIdentity) -> Markup {
    if let Some(host) = &client.verified_host {
        return html! { p class="verified" { "Verified by " (host) } };
    }

    html! {
        p class="warning" {
            strong { "This app's name isn't verified." }
            span { "Any app can call itself " (client.name) ". Go ahead only if you just connected it yourself." }
        }
    }
}

fn time_left(expires_at: i64) -> String {
    let minutes = (expires_at - now_ms()).max(0) / 60_000;

    match minutes {
        0 => "in less than a minute".to_owned(),
        1 => "in 1 minute".to_owned(),
        minutes => format!("in {minutes} minutes"),
    }
}

fn expiry(summary: &RequestSummary, label: &str) -> Markup {
    let expires_at = summary.expires_at;
    let lifetime = (expires_at - summary.opened_at).max(1);
    let left = (expires_at - now_ms()).clamp(0, lifetime);
    let permille = left * 1000 / lifetime;
    let style = format!("--left:{}.{:03};--seconds:{}s", permille / 1000, permille % 1000, left / 1000);

    html! {
        div class=(if left < 2 * 60_000 { "expiry low" } else { "expiry" }) {
            div class="track" { span style=(style) {} }
            span { (label) " " (time_left(expires_at)) }
        }
    }
}

fn plural(count: i64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn access(tools: &[SharedTool], open: bool) -> Markup {
    let mut sorted: Vec<&SharedTool> = tools.iter().collect();
    sorted.sort_by_key(|tool| std::cmp::Reverse(tool.level));
    let count = |wanted: fn(Level) -> bool| {
        i64::try_from(sorted.iter().filter(|tool| wanted(tool.level)).count()).unwrap_or(i64::MAX)
    };
    let changes = count(|level| matches!(level, Level::Changes | Level::ClientChooses));
    let runs = count(|level| level == Level::RunsCode);
    let mut summary = vec![plural(count(|_| true), "tool", "tools")];
    if changes > 0 {
        summary.push(format!("{changes} can change things"));
    }
    if runs > 0 {
        summary.push(plural(runs, "runs commands", "run commands"));
    }
    let summary = summary.join(" · ");

    html! {
        details open[open] {
            summary { "What it can use" small { (summary) } }
            @if !sorted.is_empty() {
                ul class="tools" {
                    @for tool in &sorted {
                        li {
                            code { (tool.name) }
                            @match tool.level {
                                Level::RunsCode => span class="badge" { "runs commands" },
                                Level::Changes | Level::ClientChooses => span class="badge" { "can change things" },
                                Level::Reads => {},
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn authorize_page(request_id: &str, summary: &RequestSummary, entry: &Entry) -> Markup {
    let client = &summary.client;
    let main = html! {
        main class="connect" {
            (seal("lock"))
            div class="lede" {
                @if let Entry::OtherBrowser = entry {
                    h1 { "Go back to the browser " (client.name) " opened" }
                } @else {
                    h1 { "Enter the code from " (summary.computer) }
                }
                p { "This connects " strong { (client_title(client)) } " to " strong { (summary.server_name) } "." }
            }
            @match &client.verified_host {
                Some(host) => p class="verified" { "Verified by " (host) },
                None => p class="warning" {
                    strong { "Only continue if you started this." }
                    span {
                        "Any app can call itself " (client.name) "."
                        @if let Some(host) = elsewhere(summary) { " After you connect, it sends you to " b { (host) } "." }
                    }
                },
            }
            @if let Entry::OtherBrowser = entry {
                p class="error" role="alert" {
                    "This page only works in the browser " (client.name) " opened. "
                    "Enter the code there. Nothing was used up here."
                }
            } @else {
                form class=(if matches!(entry, Entry::Wrong { .. }) { "entry wrong" } else { "entry" })
                    method="post" action="/oauth/complete-code" {
                    input type="hidden" name="req" value=(request_id);
                    label for="code" { "Code" }
                    div class="pin" {
                        div class="slots" aria-hidden="true" { @for _ in 0..8 { span {} } }
                        input id="code" name="code" value="" maxlength="9"
                            aria-describedby="code-feedback" aria-invalid=(if matches!(entry, Entry::Wrong { .. }) { "true" } else { "false" })
                            autocomplete="one-time-code" autocapitalize="characters" autocorrect="off" spellcheck="false"
                            enterkeyhint="go" required autofocus;
                    }
                    p id="code-feedback" class=(if matches!(entry, Entry::Wrong { .. }) { "feedback error" } else { "feedback hint" }) role="status" {
                        @if let Entry::Wrong { tries_left } = entry {
                            "That code didn't match. " (plural(*tries_left, "try", "tries")) " left."
                        } @else {
                            "Lowercase and dashes are fine."
                        }
                    }
                    button class="primary" type="submit" { "Connect" }
                }
                p class="hint" {
                    "It's in porchlight on " (summary.computer) ": the notification, Clients, or "
                    code { "porchlight approve" } ". Expires " (time_left(summary.expires_at)) "."
                }
            }
            div class="group" { (access(&summary.tools, false)) }
            script { (PreEscaped(SCRIPT)) }
        }
    };

    document("Connect", "", None, &main)
}

fn seal(kind: &str) -> Markup {
    html! {
        div class=(format!("seal {kind}")) aria-hidden="true" {
            svg viewBox="0 0 32 32" {
                @match kind {
                    "ok" => { path d="M8 16.5l5.5 5.5L24 11" {} }
                    "lock" => { rect x="8" y="14" width="16" height="12" rx="2" {} path d="M11 14V9a5 5 0 0 1 10 0v5" {} }
                    "denied" => { path d="M10.5 10.5l11 11M21.5 10.5l-11 11" {} }
                    _ => { circle cx="16" cy="16" r="10" {} path d="M16 10.5V16l3.5 2.5" {} }
                }
            }
        }
    }
}

pub fn ended_page(client: &str) -> Response {
    let main = html! {
        main class="done" {
            (seal("ended"))
            h1 { "This request has ended" }
            p { "It was turned down on the computer, or the code expired or was entered wrong too many times." }
            p class="trust" { "Start again from " (client) ". It will ask for a new code." }
        }
    };

    html_response(&document("Request ended", "", None, &main), StatusCode::GONE)
}

pub fn code_page(request_id: &str, summary: &RequestSummary, code: &str, wrong: i64, csrf: &str) -> Markup {
    let main = html! {
        main class="code-view" {
            div class="request-intro" {
                p class="eyebrow" { "Connection request" }
                h1 { (client_title(&summary.client)) " wants to connect to " (summary.server_name) }
                p { "Type this code on the device that is connecting. Only do it if you started this connection yourself." }
                div class="code-card" {
                    p class="label" { "Type this code on the device that's connecting" }
                    p class="code" aria-label=(code) {
                        @for letter in code.chars() {
                            @if letter == '-' { span class="dash" { "-" } } @else { span { (letter) } }
                        }
                    }
                    (expiry(summary, "Expires"))
                }
                @if wrong > 0 {
                    p class="error" role="status" {
                        "A wrong code was entered " (plural(wrong, "time", "times")) ". "
                        "If that wasn't you, deny this request."
                    }
                } @else {
                    p class="status" { span class="pulse" {} "Waiting for the code. Share it only with the device you're setting up." }
                }
                form method="post" action="/deny" {
                    input type="hidden" name="req" value=(request_id);
                    input type="hidden" name="csrf" value=(csrf);
                    button class="quiet" { "Deny this request" }
                }
            }
            div class="request-card" {
                div class="request-section" {
                    h2 { "Requested by" }
                    strong { (client_title(&summary.client)) }
                    (identity(&summary.client))
                }
                div class="request-section" {
                    h2 { "Link" }
                    strong { (summary.server_name) }
                }
                div class="request-section" {
                    (access(&summary.tools, true))
                }
            }
        }
    };

    document("Code", "this computer", Some(3), &main)
}

pub enum Outcome {
    Connected,
    Denied,
    Ended,
}

pub fn outcome_page(client: &str, server: &str, outcome: &Outcome) -> Markup {
    let main = html! {
        main class="done" {
            @match outcome {
                Outcome::Connected => {
                    (seal("ok"))
                    h1 { (client) " is connected to " (server) }
                    p { "You can close this page." }
                    p class="trust" { "Take access away any time under Clients in porchlight." }
                }
                Outcome::Denied => {
                    (seal("denied"))
                    h1 { "Denied" }
                    p { (client) " can't use " (server) ". Nothing was shared." }
                }
                Outcome::Ended => {
                    (seal("ended"))
                    h1 { "This request has ended" }
                    p { "The code expired or was entered wrong too many times. Nothing was shared." }
                    p class="trust" { "To connect, start again from " (client) "." }
                }
            }
            a class="button" href="/" { "Back to porchlight" }
        }
    };

    document("Request", "this computer", None, &main)
}

pub struct PanelLink {
    pub name: String,
    pub address: String,
}

pub struct PanelRequest {
    pub id: String,
    pub client: String,
    pub link: String,
}

pub struct Panel {
    pub links: Vec<PanelLink>,
    pub pending: Vec<PanelRequest>,
}

pub fn control_page(panel: &Panel) -> Markup {
    let main = html! {
        main class="home" {
            h1 { "Waiting for approval" }
            @if panel.pending.is_empty() {
                p class="empty" { span class="lamp" {} "Nothing is waiting. When an app connects, its code shows up here." }
            } @else {
                ul class="rows waiting group" {
                    @for request in &panel.pending {
                        li {
                            span class="who" { strong { (request.client) } small { "wants " (request.link) } }
                            a class="button primary compact" href=(format!("/approve?req={}", url::form_urlencoded::byte_serialize(request.id.as_bytes()).collect::<String>())) { "Show code" }
                        }
                    }
                }
            }
            section {
                h2 { "Links" }
                ul class="rows group" {
                    @for link in &panel.links {
                        li { strong { (link.name) } code { (link.address) } }
                    }
                }
            }
        }
    };

    document("Home", "this computer", Some(5), &main)
}
