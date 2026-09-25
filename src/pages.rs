use crate::client_metadata::ClientIdentity;
use crate::rpc::with_status;
use axum::body::Body;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderValue, StatusCode, header};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

const STYLE: &str = include_str!("../assets/style.css");

const GLOW: &str = "addEventListener('pointermove',e=>{const s=document.documentElement.style;s.setProperty('--x',e.clientX+'px');s.setProperty('--y',e.clientY+'px')},{passive:true})";

static POLICY: LazyLock<String> = LazyLock::new(|| {
    let hash = STANDARD.encode(Sha256::digest(GLOW.as_bytes()));
    format!("default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{hash}'; frame-ancestors 'none'")
});

fn secured(mut response: Response) -> Response {
    let headers = response.headers_mut();

    if let Ok(policy) = HeaderValue::from_str(&POLICY) {
        headers.insert("content-security-policy", policy);
    }

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

fn document(title: &str, place: &str, main: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width,initial-scale=1";
            title { (title) " · porchlight" }
            style { (PreEscaped(STYLE)) }
            script { (PreEscaped(GLOW)) }
            header {
                span class="mark" { "porchlight" }
                small { (place) }
            }
            (main)
        }
    }
}

pub struct Message {
    pub label: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
    pub next: &'static str,
}

pub fn message_response(status: StatusCode, message: &Message) -> Response {
    let waiting = matches!(status.as_u16(), 400 | 429);
    let main = html! {
        main {
            hgroup {
                p class=(if waiting { "status label wait" } else { "status label" }) {
                    (status.as_u16()) " · " (message.label)
                }
                h1 { (message.title) }
            }
            p class="detail" { (message.detail) }
            p class="next" { span class="label" { "next" } (message.next) }
        }
    };

    html_response(&document(message.title, "", &main), status)
}

pub struct RequestSummary {
    pub client: ClientIdentity,
    pub server_name: String,
    pub tool_count: Option<usize>,
    pub redirect_uri: String,
}

fn return_host(summary: &RequestSummary) -> String {
    url::Url::parse(&summary.redirect_uri).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default()
}

fn returns_to(summary: &RequestSummary, verb: &str) -> String {
    let host = return_host(summary);

    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        format!("{verb} returns to an app on the device that started this. only continue if that was you")
    } else {
        format!("{verb} returns you to {host}")
    }
}

struct Connection {
    path: Markup,
    ledger: Markup,
    warning: Markup,
    client_title: String,
}

fn connection(summary: &RequestSummary) -> Connection {
    let client = &summary.client;
    let verified = client.verified_host.is_some();
    let starting = summary.tool_count.is_none();
    let tools = summary
        .tool_count
        .map_or_else(|| " · still starting, tools appear once it's up".to_owned(), |count| format!(" · {count} tools"));
    let badge = if let Some(host) = &client.verified_host {
        html! { span class="badge verified" { "✓ verified · " (host) } }
    } else {
        html! { span class="badge unverified" { "name not verified" } }
    };

    Connection {
        path: html! {
            nav class="path" {
                span class=(if verified { "" } else { "unverified" }) { i {} (client.name) }
                div class="wire" {}
                b {}
                div class="wire" {}
                span { i class=(if starting { "starting" } else { "live" }) {} (summary.server_name) }
            }
        },
        ledger: html! {
            dl {
                div { dt { "asked by" } dd { (client.name) " " (badge) } }
                div { dt { "wants" } dd { (summary.server_name) (tools) } }
            }
        },
        warning: if verified {
            html! {}
        } else {
            html! {
                p class="warning" {
                    strong { "this app's name isn't verified" }
                    "anyone can call their app “" (client.name) "”. only continue if you just added this connector yourself and expect to return to " (return_host(summary)) "."
                }
            }
        },
        client_title: if verified { client.name.clone() } else { format!("“{}”", client.name) },
    }
}

pub fn authorize_page(request_id: &str, summary: &RequestSummary, approval_port: u16) -> Markup {
    let shared = connection(summary);
    let main = html! {
        main class="connect" {
            (shared.path)
            hgroup {
                p class="label" { "step 1 of 2 · connect" }
                h1 { "connect " (shared.client_title) " to " (summary.server_name) }
            }
            (shared.ledger)
            (shared.warning)
            form class="approve" action=(format!("http://127.0.0.1:{approval_port}/approve")) {
                input type="hidden" name="req" value=(request_id);
                button class="primary wide" { "approve on this computer " span { "→" } }
                p class="note" { (returns_to(summary, "approving")) "." }
            }
            details {
                summary { "on a phone or another computer? " span { "use a code instead" } }
                ol {
                    li { "on the computer running porchlight, open porchlight" }
                    li { "open the " code { "clients" } " tab and press " code { "c" } " on this request" }
                    li { "enter the code it shows" }
                }
                form class="entry" method="post" action="/oauth/complete-code" {
                    input type="hidden" name="req" value=(request_id);
                    input id="code" name="code" placeholder="XXXX-XXXX" maxlength="9" autocomplete="one-time-code"
                        autocapitalize="characters" spellcheck="false" aria-label="Code" required;
                    button class="primary" { "connect " span { "→" } }
                }
            }
        }
    };

    document("Connect", "connection request", &main)
}

pub struct SharedTool {
    pub name: String,
    pub dangerous: bool,
}

pub fn approval_page(
    request_id: &str,
    summary: &RequestSummary,
    tools: &[SharedTool],
    hostname: &str,
    approval_port: u16,
    csrf: &str,
) -> Markup {
    let shared = connection(summary);
    let mut sorted: Vec<&SharedTool> = tools.iter().collect();
    sorted.sort_by_key(|tool| !tool.dangerous);
    let main = html! {
        main {
            (shared.path)
            hgroup {
                p class="label" { "step 2 of 2 · approve" }
                h1 { "allow " (shared.client_title) " to use " (summary.server_name) "?" }
            }
            (shared.ledger)
            (shared.warning)
            @if !sorted.is_empty() {
                details class="tools" open {
                    summary { "tools it can use" }
                    ul {
                        @for tool in &sorted {
                            @if tool.dangerous {
                                li class="danger" { code { (tool.name) } " can run commands or code" }
                            } @else {
                                li { code { (tool.name) } }
                            }
                        }
                    }
                }
            }
            form method="post" action=(format!("http://127.0.0.1:{approval_port}/approve")) {
                input type="hidden" name="req" value=(request_id);
                input type="hidden" name="csrf" value=(csrf);
                div class="buttons" {
                    button class="primary" name="decision" value="allow" { "allow " span { "→" } }
                    button name="decision" value="deny" { "deny" }
                }
                p class="note" { (returns_to(summary, "allowing")) ". undo it anytime in porchlight's clients tab." }
            }
            p class="local" { "this page only opens on this computer. the tunnel can't reach it, so nobody else can press allow." }
        }
    };

    document("Approve", &format!("porchlight on {hostname}"), &main)
}

pub struct PanelLink {
    pub name: String,
    pub address: String,
    pub state: &'static str,
    pub tools: usize,
    pub clients: usize,
}

pub struct PanelClient {
    pub id: String,
    pub name: String,
    pub link: String,
    pub last_used: String,
}

pub struct PanelRequest {
    pub id: String,
    pub client: String,
    pub link: String,
}

pub struct Panel {
    pub host: String,
    pub csrf: String,
    pub links: Vec<PanelLink>,
    pub problems: Vec<String>,
    pub pending: Vec<PanelRequest>,
    pub clients: Vec<PanelClient>,
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 { format!("1 {word}") } else { format!("{count} {word}s") }
}

pub fn control_page(panel: &Panel) -> Markup {
    let headline = match panel.pending.len() {
        0 if panel.links.is_empty() => "nothing shared yet".to_owned(),
        0 => "the light is on".to_owned(),
        count => format!("{} at the door", plural(count, "client")),
    };
    let main = html! {
        main class="home" {
            hgroup {
                p class="label" { "on " (panel.host) }
                h1 { (headline) }
            }
            @if !panel.pending.is_empty() {
                ul class="rows waiting" {
                    @for request in &panel.pending {
                        li {
                            span { strong { (request.client) } " " small { "wants " (request.link) } }
                            a class="primary" href=(format!("/approve?req={}", url::form_urlencoded::byte_serialize(request.id.as_bytes()).collect::<String>())) { "review →" }
                        }
                    }
                }
            }
            section {
                h2 { "links" }
                ul class="rows" {
                    @for problem in &panel.problems {
                        li class="bad" { "✗ " (problem) }
                    }
                    @if panel.links.is_empty() {
                        li { small { "run porchlight to share an app" } }
                    }
                    @for link in &panel.links {
                        li {
                            span { i class=(link.state) {} strong { (link.name) } " " small { (plural(link.tools, "tool")) " · " (plural(link.clients, "client")) } }
                            code { (link.address) }
                        }
                    }
                }
            }
            @if !panel.clients.is_empty() {
                section {
                    h2 { "clients" }
                    ul class="rows" {
                        @for client in &panel.clients {
                            li {
                                span { (client.name) " " small { (client.link) " · " (client.last_used) } }
                                form method="post" action="/revoke" {
                                    input type="hidden" name="csrf" value=(panel.csrf);
                                    input type="hidden" name="id" value=(client.id);
                                    button { "revoke" }
                                }
                            }
                        }
                    }
                }
            }
            p class="local" { "tools, logs and the tunnel live in the porchlight app. this page only opens on this computer." }
        }
    };

    document("home", "", &main)
}
