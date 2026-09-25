use crate::config::{McpServer, Settings, Words};
use futures::future::join_all;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct KnownServer {
    id: &'static str,
    display_name: &'static str,
    url: &'static str,
}

const KNOWN_SERVERS: [KnownServer; 1] =
    [KnownServer { id: "paper", display_name: "Paper Desktop", url: "http://127.0.0.1:29979/mcp" }];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Http,
    Stdio,
    Cli,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredServer {
    pub name: String,
    pub display_name: String,
    pub source: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
    pub running: bool,
    pub local: bool,
}

impl DiscoveredServer {
    fn identity(&self) -> String {
        self.url
            .clone()
            .or_else(|| self.command.as_ref().map(|command| command.join(" ")))
            .unwrap_or_else(|| self.name.clone())
    }

    pub fn exposable(&self) -> bool {
        self.local && (self.kind != Kind::Http || self.running)
    }

    pub fn entry(&self) -> Option<McpServer> {
        if let Some(url) = &self.url {
            return Some(McpServer::Http(crate::config::HttpServer {
                url: url.clone(),
                headers: None,
                enabled: None,
                allow_dangerous: None,
            }));
        }

        self.command.as_ref().map(|command| {
            McpServer::Stdio(crate::config::StdioServer {
                command: Words::Many(command.clone()),
                environment: self.env.clone(),
                cwd: None,
                enabled: None,
                allow_dangerous: None,
            })
        })
    }
}

fn local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 127
                || a == 10
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && b == 168)
                || (a == 169 && b == 254)
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments().first().is_some_and(|first| (first & 0xfe00) == 0xfc00),
    }
}

pub fn is_local_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');

    host == "localhost" || host.rsplit('.').next() == Some("local") || host.parse::<IpAddr>().is_ok_and(local_address)
}

#[derive(Deserialize)]
struct Entry {
    url: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<IndexMap<String, String>>,
}

#[derive(Deserialize)]
struct Nested {
    servers: Option<IndexMap<String, Entry>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpConfig {
    mcp_servers: Option<IndexMap<String, Entry>>,
    servers: Option<IndexMap<String, Entry>>,
    mcp: Option<Nested>,
}

fn config_files(cwd: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = std::env::home_dir().unwrap_or_default();

    vec![
        ("claude-desktop", home.join("Library/Application Support/Claude/claude_desktop_config.json")),
        ("claude-desktop", home.join(".config/Claude/claude_desktop_config.json")),
        ("claude-code", home.join(".claude.json")),
        ("claude-code-project", cwd.join(".mcp.json")),
        ("cursor", home.join(".cursor/mcp.json")),
        ("cursor-project", cwd.join(".cursor/mcp.json")),
        ("vscode-project", cwd.join(".vscode/mcp.json")),
    ]
}

pub async fn probe(url: &str) -> bool {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "porchlight", "version": "0.1.0" } },
    });
    let Ok(client) =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(3)).build()
    else {
        return false;
    };

    client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string())
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn read_config(path: &Path, source: &str) -> Vec<DiscoveredServer> {
    let Some(config) =
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<McpConfig>(&text).ok())
    else {
        return Vec::new();
    };
    let mut entries: IndexMap<String, Entry> = IndexMap::new();
    entries.extend(config.mcp.and_then(|nested| nested.servers).unwrap_or_default());
    entries.extend(config.servers.unwrap_or_default());
    entries.extend(config.mcp_servers.unwrap_or_default());

    entries
        .into_iter()
        .filter_map(|(name, entry)| {
            let base = |kind| DiscoveredServer {
                display_name: name.clone(),
                name: name.clone(),
                source: source.to_owned(),
                kind,
                url: None,
                command: None,
                env: None,
                running: false,
                local: true,
            };

            if let Some(url) = entry.url {
                return Some(DiscoveredServer { local: is_local_url(&url), url: Some(url), ..base(Kind::Http) });
            }

            entry.command.map(|command| DiscoveredServer {
                command: Some(std::iter::once(command).chain(entry.args.unwrap_or_default()).collect()),
                env: entry.env,
                ..base(Kind::Stdio)
            })
        })
        .collect()
}

pub async fn discover(settings: &Settings) -> Vec<DiscoveredServer> {
    let own = |name: &String, kind: Kind| DiscoveredServer {
        name: name.clone(),
        display_name: name.clone(),
        source: "porchlight.json".to_owned(),
        kind,
        url: None,
        command: None,
        env: None,
        running: kind == Kind::Cli,
        local: true,
    };
    let configured = settings.mcp.iter().map(|(name, server)| match server {
        McpServer::Http(http) => DiscoveredServer { url: Some(http.url.clone()), ..own(name, Kind::Http) },
        McpServer::Stdio(stdio) => DiscoveredServer {
            command: Some(match &stdio.command {
                Words::One(text) => vec![text.clone()],
                Words::Many(words) => words.clone(),
            }),
            ..own(name, Kind::Stdio)
        },
    });
    let commands = settings.commands.keys().map(|name| own(name, Kind::Cli));
    let known = KNOWN_SERVERS.iter().map(|server| DiscoveredServer {
        name: server.id.to_owned(),
        display_name: server.display_name.to_owned(),
        source: "known".to_owned(),
        kind: Kind::Http,
        url: Some(server.url.to_owned()),
        command: None,
        env: None,
        running: false,
        local: true,
    });
    let cwd = std::env::current_dir().unwrap_or_default();
    let found = config_files(&cwd).into_iter().flat_map(|(source, path)| read_config(&path, source));
    let mut unique: Vec<DiscoveredServer> = Vec::new();

    for server in configured.chain(commands).chain(known).chain(found) {
        if !unique.iter().any(|other| other.identity() == server.identity()) {
            unique.push(server);
        }
    }

    let probed = join_all(unique.into_iter().map(|server| async move {
        match &server.url {
            Some(url) => {
                let running = probe(url).await;
                DiscoveredServer { running, ..server }
            }
            None => server,
        }
    }))
    .await;

    probed.into_iter().filter(|server| server.source != "known" || server.running).collect()
}
