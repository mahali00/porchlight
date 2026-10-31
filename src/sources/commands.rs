use super::process::process_environment;
use super::{Notices, SourceRequest};
use crate::config::Input;
use crate::core::{Annotations, Tool, ToolResult};
use crate::policy::Risk;
use crate::rpc::{self, decode_batch, empty, list_notification_stream, parse_error, rpc_error, rpc_result};
use axum::response::Response;
use futures::future::join_all;
use http::{HeaderValue, Method, StatusCode, header};
use indexmap::IndexMap;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
mod inputs;
mod risk;
mod template;

use inputs::input_schema;
pub use inputs::validate;
pub use risk::{assess, runs_client_code};
pub use template::{Lookup, LookupKind, Part, Word, fill, inputs_of, literal, parse_word, sole_input};

#[derive(Clone, Debug, PartialEq)]
pub struct CommandTool {
    pub words: Vec<Word>,
    pub template: String,
    pub description: Option<String>,
    pub read_only: bool,
    pub resource: bool,
    pub inputs: IndexMap<String, Input>,
    pub environment: IndexMap<String, String>,
    pub cwd: Option<String>,
}

const TIMEOUT: Duration = Duration::from_mins(1);

const MAX_OUTPUT: usize = 100_000;

impl CommandTool {
    pub fn risk(&self) -> Risk {
        assess(&self.words, &self.inputs, self.read_only)
    }
}

fn tool_for(name: &str, tool: &CommandTool) -> Tool {
    Tool {
        name: name.to_owned(),
        description: Some(tool.description.clone().unwrap_or_else(|| format!("Runs {}", tool.template))),
        annotations: Some(Annotations {
            read_only_hint: Some(tool.risk().reads()),
            destructive_hint: Some(!tool.risk().reads()),
        }),
        input_schema: Some(input_schema(tool)),
    }
}

pub fn command_tools(tools: &IndexMap<String, CommandTool>) -> Vec<Tool> {
    tools.iter().map(|(name, tool)| tool_for(name, tool)).collect()
}

async fn collect(mut output: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 8192];

    while let Ok(read) = output.read(&mut chunk).await {
        if read == 0 {
            break;
        }

        if kept.len() <= MAX_OUTPUT {
            kept.extend(chunk.iter().take(read));
        }
    }

    let text = String::from_utf8_lossy(&kept).into_owned();

    match text.char_indices().nth(MAX_OUTPUT) {
        Some((end, _)) => format!("{}\n… output cut off", text.get(..end).unwrap_or_default()),
        None => text.trim().to_owned(),
    }
}

async fn execute(argv: &[String], tool: &CommandTool) -> ToolResult {
    let Some((program, arguments)) = argv.split_first() else {
        return ToolResult { text: "Couldn't run the command".to_owned(), is_error: true };
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .envs(process_environment(&tool.environment))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);

    if let Some(cwd) = &tool.cwd {
        command.current_dir(cwd);
    }

    let Ok(mut child) = command.spawn() else {
        return ToolResult { text: format!("Couldn't run {program}"), is_error: true };
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return ToolResult { text: format!("Couldn't read the output of {program}"), is_error: true };
    };
    let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
    let finished = async {
        let (stdout, stderr, status) = tokio::join!(collect(stdout), collect(stderr), child.wait());
        (stdout, stderr, status)
    };

    match tokio::time::timeout(TIMEOUT, finished).await {
        Err(_) => {
            if let Some(pid) = pid {
                let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            }
            ToolResult { text: "Stopped after 60 seconds".to_owned(), is_error: true }
        }
        Ok((_, _, Err(_))) => ToolResult { text: format!("Couldn't read the output of {program}"), is_error: true },
        Ok((stdout, _, Ok(status))) if status.success() => {
            ToolResult { text: if stdout.is_empty() { "Done.".to_owned() } else { stdout }, is_error: false }
        }
        Ok((stdout, stderr, Ok(status))) => {
            let joined: Vec<String> = [stdout, stderr].into_iter().filter(|text| !text.is_empty()).collect();
            let text = if joined.is_empty() {
                status
                    .code()
                    .map_or_else(|| "Stopped by a signal".to_owned(), |code| format!("Exited with code {code}"))
            } else {
                joined.join("\n")
            };

            ToolResult { text, is_error: true }
        }
    }
}

pub struct Run {
    pub text: String,
    pub is_error: bool,
    pub invalid: bool,
}

pub async fn run_tool(tool: &CommandTool, arguments: &Value) -> Run {
    match validate(tool, arguments) {
        Err(message) => Run { text: format!("Invalid arguments: {message}"), is_error: true, invalid: true },
        Ok(values) => {
            let result = execute(&fill(&tool.words, &values, &tool.inputs), tool).await;
            Run { text: result.text, is_error: result.is_error, invalid: false }
        }
    }
}

fn structured(text: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(text).ok()? {
        Value::Array(items) => Some(json!({ "items": items })),
        object @ Value::Object(_) => Some(object),
        _ => None,
    }
}

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default, deserialize_with = "rpc::present")]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

struct ResourceItem {
    uri: String,
    name: String,
    description: String,
}

pub struct CommandsSpec {
    pub name: String,
    pub tools: IndexMap<String, CommandTool>,
    pub uri_of: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub instructions: Option<String>,
    pub notices: Option<Notices>,
}

pub struct CommandSource {
    pub name: String,
    pub shared: Vec<Tool>,
    tools: IndexMap<String, CommandTool>,
    listed: Vec<Tool>,
    resources: Vec<ResourceItem>,
    instructions: Option<String>,
    notices: Option<Notices>,
}

impl CommandSource {
    pub fn new(spec: CommandsSpec) -> Self {
        let shared = command_tools(&spec.tools);
        let listed = shared
            .iter()
            .filter(|tool| spec.tools.get(&tool.name).is_none_or(|found| !found.resource))
            .cloned()
            .collect();
        let resources = spec
            .tools
            .iter()
            .filter(|(_, tool)| tool.resource)
            .map(|(name, tool)| ResourceItem {
                uri: (spec.uri_of)(name),
                name: name.clone(),
                description: tool.description.clone().unwrap_or_else(|| format!("Output of {}", tool.template)),
            })
            .collect();

        Self {
            name: spec.name,
            shared,
            tools: spec.tools,
            listed,
            resources,
            instructions: spec.instructions,
            notices: spec.notices,
        }
    }

    async fn call(&self, request: &RpcRequest) -> Value {
        let id = request.id.as_ref();
        let params = request.params.as_ref();
        let found = params
            .and_then(|params| params.get("name"))
            .and_then(Value::as_str)
            .and_then(|name| self.tools.get(name))
            .filter(|tool| !tool.resource);
        let Some(tool) = found else {
            return rpc_error(id, -32602, "Unknown tool");
        };
        let arguments = params.and_then(|params| params.get("arguments")).cloned().unwrap_or_else(|| json!({}));
        let run = run_tool(tool, &arguments).await;

        if run.invalid {
            return rpc_error(id, -32602, &run.text);
        }

        let data = if run.is_error { None } else { structured(&run.text) };
        let mut result = json!({ "content": [{ "type": "text", "text": run.text }], "isError": run.is_error });

        if let (Some(data), Some(object)) = (data, result.as_object_mut()) {
            object.insert("structuredContent".into(), data);
        }

        rpc_result(id, result)
    }

    async fn read(&self, request: &RpcRequest) -> Value {
        let id = request.id.as_ref();
        let uri = request.params.as_ref().and_then(|params| params.get("uri")).and_then(Value::as_str);
        let found = uri.and_then(|uri| self.resources.iter().find(|resource| resource.uri == uri));
        let Some((resource, tool)) = found.and_then(|resource| Some((resource, self.tools.get(&resource.name)?)))
        else {
            return rpc_error(id, -32002, "Resource not found");
        };
        let run = run_tool(tool, &json!({})).await;

        if run.is_error {
            return rpc_error(id, -32603, &run.text);
        }

        let mime = if structured(&run.text).is_some() { "application/json" } else { "text/plain" };

        rpc_result(
            id,
            json!({ "contents": [{ "uri": resource.uri, "mimeType": mime, "text": run.text }], "ttlMs": 0, "cacheScope": "private" }),
        )
    }

    fn initialize(&self, request: &RpcRequest) -> Value {
        let change = if self.notices.is_some() { json!({ "listChanged": true }) } else { json!({}) };
        let version = request
            .params
            .as_ref()
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or("2025-06-18");
        let mut capabilities = json!({ "tools": change.clone() });
        let mut result = json!({
            "protocolVersion": version,
            "capabilities": {},
            "serverInfo": { "name": self.name, "version": crate::core::VERSION },
        });

        if !self.resources.is_empty()
            && let Some(object) = capabilities.as_object_mut()
        {
            object.insert("resources".into(), change);
        }

        if let Some(object) = result.as_object_mut() {
            object.insert("capabilities".into(), capabilities);

            if let Some(instructions) = self.instructions.as_ref().filter(|text| !text.is_empty()) {
                object.insert("instructions".into(), json!(instructions));
            }
        }

        rpc_result(request.id.as_ref(), result)
    }

    async fn answer(&self, request: &RpcRequest) -> Value {
        let id = request.id.as_ref();

        match request.method.as_str() {
            "initialize" => self.initialize(request),
            "server/discover" => rpc_result(
                id,
                json!({
                    "protocolVersions": ["2025-06-18", "2025-11-25", "2026-07-28"],
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": self.name, "version": crate::core::VERSION }
                }),
            ),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": self.listed, "ttlMs": 0, "cacheScope": "private" })),
            "tools/call" => self.call(request).await,
            "resources/list" => {
                let resources: Vec<Value> = self
                    .resources
                    .iter()
                    .map(|item| json!({ "uri": item.uri, "name": item.name, "description": item.description }))
                    .collect();
                rpc_result(id, json!({ "resources": resources, "ttlMs": 0, "cacheScope": "private" }))
            }
            "resources/read" => self.read(request).await,
            method => rpc_error(id, -32601, &format!("Method not found: {method}")),
        }
    }

    pub async fn handle(&self, request: SourceRequest) -> Response {
        if request.method == Method::GET
            && let Some(notices) = &self.notices
        {
            return list_notification_stream(notices());
        }

        if request.method != Method::POST {
            let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
            let allow = if self.notices.is_some() { "GET, POST" } else { "POST" };
            response.headers_mut().insert(header::ALLOW, HeaderValue::from_static(allow));
            return response;
        }

        let Some(decoded) = decode_batch::<RpcRequest>(&request.body) else {
            return parse_error();
        };
        let requests: Vec<&RpcRequest> = decoded.items.iter().filter(|item| item.id.is_some()).collect();
        let mut replies = join_all(requests.into_iter().map(|item| self.answer(item))).await;

        match (decoded.batch, replies.len()) {
            (_, 0) => empty(StatusCode::ACCEPTED),
            (true, _) => rpc::json(&Value::Array(replies), StatusCode::OK),
            (false, _) => rpc::json(&replies.swap_remove(0), StatusCode::OK),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<Word> {
        words
            .iter()
            .map(|word| {
                if word.starts_with('{') && word.ends_with('}') {
                    vec![Part::Input(word.trim_matches(['{', '}']).to_owned())]
                } else {
                    vec![Part::Text((*word).to_owned())]
                }
            })
            .collect()
    }

    #[test]
    fn classifies_dispatchers_and_interpreter_flags_with_client_input_as_code() {
        assert!(runs_client_code(&words(&["busybox", "sh", "-c", "{script}"])));
        assert!(runs_client_code(&words(&["toybox", "awk", "{code}"])));
        assert!(runs_client_code(&words(&["python3", "-c", "{script}"])));
        assert!(runs_client_code(&words(&["env", "python", "-e", "{script}"])));
        assert!(!runs_client_code(&words(&["echo", "{text}"])));
        assert!(!runs_client_code(&words(&["python", "-c", "trusted literal"])));
    }
}
