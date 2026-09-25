use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotations {
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
    #[serde(rename = "inputSchema", default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub app: String,
    pub message: String,
}

pub const RESERVED_NAMES: [&str; 3] = ["porchlight", "oauth", ".well-known"];

pub fn link_path(link: &str) -> String {
    format!("/{link}/mcp")
}

pub fn exposed_name(app: &str, tool: &str) -> String {
    if tool == app { app.to_owned() } else { format!("{app}_{tool}") }
}

pub fn resource_uri(app: &str, tool: &str) -> String {
    format!("porchlight://{app}/{tool}")
}

pub fn is_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn is_app_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());

    first_ok && name.len() <= 40 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn is_usable_name(name: &str) -> bool {
    is_app_name(name) && !RESERVED_NAMES.contains(&name)
}

pub fn is_http_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| url.scheme() == "http" || url.scheme() == "https")
}

pub fn is_https_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| url.scheme() == "https")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

pub fn ago(timestamp: Option<i64>, now: i64) -> String {
    let Some(at) = timestamp else {
        return "not used yet".to_owned();
    };
    let minutes = (now - at + 30_000) / 60_000;

    if minutes < 2 {
        return "active now".to_owned();
    }

    if minutes < 60 {
        return format!("{minutes} min ago");
    }

    if minutes < 60 * 24 {
        return format!("{} h ago", (minutes + 30) / 60);
    }

    match (minutes + 720) / (60 * 24) {
        1 => "yesterday".to_owned(),
        days => format!("{days} days ago"),
    }
}

pub fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", text.get(..end).unwrap_or(text)),
        None => text.to_owned(),
    }
}
