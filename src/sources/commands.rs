use super::process::process_environment;
use super::{Notices, SourceRequest};
use crate::config::{Input, InputType};
use crate::core::{Annotations, Tool, ToolResult};
use crate::rpc::{self, decode_batch, empty, list_notification_stream, parse_error, rpc_error, rpc_result};
use axum::response::Response;
use futures::future::join_all;
use http::{HeaderValue, Method, StatusCode, header};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Input(String),
}

pub type Word = Vec<Part>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupKind {
    Env,
    File,
}

pub type Lookup<'a> = &'a dyn Fn(LookupKind, &str) -> Option<String>;

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

const MAX_INPUT: usize = 4_096;

fn push_text(word: &mut Word, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Some(Part::Text(last)) = word.last_mut() {
        last.push_str(text);
    } else {
        word.push(Part::Text(text.to_owned()));
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

enum Reference<'a> {
    Lookup(LookupKind, &'a str),
    Input(&'a str),
}

fn reference(inner: &str) -> Option<Reference<'_>> {
    if let Some(name) = inner.strip_prefix("env:").filter(|name| !name.is_empty()) {
        return Some(Reference::Lookup(LookupKind::Env, name));
    }

    if let Some(name) = inner.strip_prefix("file:").filter(|name| !name.is_empty()) {
        return Some(Reference::Lookup(LookupKind::File, name));
    }

    (!inner.is_empty() && inner.chars().all(is_word_char)).then_some(Reference::Input(inner))
}

pub fn parse_word(word: &str, lookup: Lookup) -> Result<Word, String> {
    let mut parts = Word::new();
    let mut rest = word;

    while let Some(open) = rest.find('{') {
        let (before, from_brace) = rest.split_at(open);
        push_text(&mut parts, before);
        let after_brace = from_brace.get(1..).unwrap_or_default();
        let found = after_brace.find('}').and_then(|close| {
            let inner = after_brace.get(..close)?;
            Some((reference(inner)?, after_brace.get(close + 1..).unwrap_or_default()))
        });

        match found {
            Some((Reference::Input(name), tail)) => {
                parts.push(Part::Input(name.to_owned()));
                rest = tail;
            }
            Some((Reference::Lookup(kind, name), tail)) => {
                let value = lookup(kind, name).ok_or_else(|| match kind {
                    LookupKind::Env => format!("{name} isn't set"),
                    LookupKind::File => format!("can't read {name}"),
                })?;
                push_text(&mut parts, &value);
                rest = tail;
            }
            None => {
                push_text(&mut parts, "{");
                rest = after_brace;
            }
        }
    }

    push_text(&mut parts, rest);

    Ok(parts)
}

pub fn literal(word: &Word) -> String {
    word.iter()
        .map(|part| match part {
            Part::Text(text) => text.clone(),
            Part::Input(name) => format!("{{{name}}}"),
        })
        .collect()
}

pub fn inputs_of(words: &[Word]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    for part in words.iter().flatten() {
        if let Part::Input(name) = part
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }

    names
}

pub fn sole_input(word: &Word) -> Option<&str> {
    match word.as_slice() {
        [Part::Input(name)] => Some(name),
        _ => None,
    }
}

fn has_input(word: &Word) -> bool {
    word.iter().any(|part| matches!(part, Part::Input(_)))
}

fn runs_anything(name: &str) -> bool {
    const SHELLS: [&str; 8] = ["sh", "bash", "zsh", "fish", "csh", "ksh", "tcsh", "dash"];
    const OTHERS: [&str; 20] = [
        "node",
        "bun",
        "deno",
        "ruby",
        "perl",
        "php",
        "lua",
        "awk",
        "gawk",
        "mawk",
        "nawk",
        "osascript",
        "pwsh",
        "powershell",
        "env",
        "xargs",
        "sudo",
        "ssh",
        "eval",
        "python",
    ];

    SHELLS.contains(&name)
        || OTHERS.contains(&name)
        || name.strip_prefix("python").is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

pub fn runs_client_code(words: &[Word]) -> bool {
    let Some(program) = words.first() else {
        return false;
    };

    if has_input(program) {
        return true;
    }

    let text: String = program
        .iter()
        .filter_map(|part| if let Part::Text(text) = part { Some(text.as_str()) } else { None })
        .collect();
    let name = Path::new(&text).file_name().and_then(|name| name.to_str()).unwrap_or_default();

    runs_anything(name) && words.iter().any(has_input)
}

fn number_text(number: &serde_json::Number) -> String {
    match number.as_f64() {
        Some(float) if !number.is_i64() && !number.is_u64() && float.fract() == 0.0 && float.abs() < 1e21 => {
            format!("{float:.0}")
        }
        _ => number.to_string(),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number_text(number),
        other => other.to_string(),
    }
}

fn fill_word(word: &Word, values: &Map<String, Value>, inputs: &IndexMap<String, Input>) -> Option<String> {
    if word.iter().any(|part| matches!(part, Part::Input(name) if !values.contains_key(name))) {
        return None;
    }

    if let Some(name) = sole_input(word)
        && let Some(flag) = inputs.get(name).and_then(|input| input.flag.as_ref())
    {
        return (values.get(name) == Some(&Value::Bool(true))).then(|| flag.clone());
    }

    Some(
        word.iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Input(name) => values.get(name).map(value_text).unwrap_or_default(),
            })
            .collect(),
    )
}

pub fn fill(words: &[Word], values: &Map<String, Value>, inputs: &IndexMap<String, Input>) -> Vec<String> {
    words.iter().filter_map(|word| fill_word(word, values, inputs)).collect()
}

fn starts_word(words: &[Word], name: &str) -> bool {
    words.iter().any(|word| matches!(word.first(), Some(Part::Input(first)) if first == name))
}

fn check_number(name: &str, value: &Value, input: Option<&Input>, integer: bool, starts: bool) -> Result<(), String> {
    let number = value.as_f64().filter(|number| number.is_finite()).ok_or(format!("{name}: must be a number"))?;

    if integer && number.fract() != 0.0 {
        return Err(format!("{name}: must be a whole number"));
    }

    if let Some(min) = input.and_then(|input| input.min).filter(|min| number < *min) {
        return Err(format!("{name}: must be at least {min}"));
    }

    if let Some(max) = input.and_then(|input| input.max).filter(|max| number > *max) {
        return Err(format!("{name}: must be at most {max}"));
    }

    if starts && number < 0.0 {
        return Err(format!("{name}: can't be negative here"));
    }

    Ok(())
}

fn check_value(name: &str, value: &Value, input: Option<&Input>, starts: bool) -> Result<(), String> {
    match input.and_then(|input| input.kind) {
        Some(InputType::Boolean) => value.is_boolean().then_some(()).ok_or(format!("{name}: must be true or false")),
        Some(InputType::Integer) => check_number(name, value, input, true, starts),
        Some(InputType::Number) => check_number(name, value, input, false, starts),
        Some(InputType::String) | None => {
            let text = value.as_str().ok_or(format!("{name}: must be text"))?;

            if let Some(choices) = input.and_then(|input| input.choices.as_ref()) {
                return choices
                    .iter()
                    .any(|choice| choice == text)
                    .then_some(())
                    .ok_or(format!("{name}: must be one of {}", choices.join(", ")));
            }

            if text.chars().count() > MAX_INPUT {
                return Err(format!("{name}: must be at most {MAX_INPUT} characters"));
            }

            if starts && (text.is_empty() || text.starts_with('-')) {
                return Err(format!("{name}: can't be empty or start with \"-\""));
            }

            Ok(())
        }
    }
}

pub fn validate(tool: &CommandTool, arguments: &Value) -> Result<Map<String, Value>, String> {
    let given = arguments.as_object().ok_or("arguments must be an object")?;
    let mut values = Map::new();

    for name in inputs_of(&tool.words) {
        let input = tool.inputs.get(&name);

        match given.get(&name) {
            None if input.is_some_and(Input::is_optional) => {}
            None => return Err(format!("{name}: is missing")),
            Some(value) => {
                check_value(&name, value, input, starts_word(&tool.words, &name))?;
                values.insert(name, value.clone());
            }
        }
    }

    Ok(values)
}

fn property(input: Option<&Input>, starts: bool) -> Value {
    let mut schema = Map::new();

    match input.and_then(|input| input.kind) {
        Some(InputType::Boolean) => {
            schema.insert("type".into(), json!("boolean"));
        }
        Some(kind @ (InputType::Integer | InputType::Number)) => {
            schema.insert("type".into(), json!(if kind == InputType::Integer { "integer" } else { "number" }));
            let floor = if starts { Some(0.0) } else { None };
            let minimum = [input.and_then(|input| input.min), floor].into_iter().flatten().reduce(f64::max);

            if let Some(minimum) = minimum {
                schema.insert("minimum".into(), json!(minimum));
            }

            if let Some(maximum) = input.and_then(|input| input.max) {
                schema.insert("maximum".into(), json!(maximum));
            }
        }
        Some(InputType::String) | None => {
            schema.insert("type".into(), json!("string"));

            if let Some(choices) = input.and_then(|input| input.choices.as_ref()) {
                schema.insert("enum".into(), json!(choices));
            } else {
                schema.insert("maxLength".into(), json!(MAX_INPUT));

                if starts {
                    schema.insert("pattern".into(), json!("^[^-]"));
                }
            }
        }
    }

    if let Some(description) = input.and_then(|input| input.description.as_ref()) {
        schema.insert("description".into(), json!(description));
    }

    Value::Object(schema)
}

fn input_schema(tool: &CommandTool) -> Value {
    let names = inputs_of(&tool.words);

    if names.is_empty() {
        return json!({ "type": "object", "properties": {} });
    }

    let properties: Map<String, Value> = names
        .iter()
        .map(|name| (name.clone(), property(tool.inputs.get(name), starts_word(&tool.words, name))))
        .collect();
    let required: Vec<&String> =
        names.iter().filter(|name| !tool.inputs.get(*name).is_some_and(Input::is_optional)).collect();

    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn tool_for(name: &str, tool: &CommandTool) -> Tool {
    Tool {
        name: name.to_owned(),
        description: Some(tool.description.clone().unwrap_or_else(|| format!("Runs {}", tool.template))),
        annotations: Some(Annotations {
            read_only_hint: Some(tool.read_only),
            destructive_hint: Some(!tool.read_only),
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
        .kill_on_drop(true);

    if let Some(cwd) = &tool.cwd {
        command.current_dir(cwd);
    }

    let Ok(mut child) = command.spawn() else {
        return ToolResult { text: format!("Couldn't run {program}"), is_error: true };
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return ToolResult { text: format!("Couldn't read the output of {program}"), is_error: true };
    };
    let finished = async {
        let (stdout, stderr, status) = tokio::join!(collect(stdout), collect(stderr), child.wait());
        (stdout, stderr, status)
    };

    match tokio::time::timeout(TIMEOUT, finished).await {
        Err(_) => ToolResult { text: "Stopped after 60 seconds".to_owned(), is_error: true },
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

        rpc_result(id, json!({ "contents": [{ "uri": resource.uri, "mimeType": mime, "text": run.text }] }))
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
            "serverInfo": { "name": self.name, "version": "1.0.0" },
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
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": self.listed })),
            "tools/call" => self.call(request).await,
            "resources/list" => {
                let resources: Vec<Value> = self
                    .resources
                    .iter()
                    .map(|item| json!({ "uri": item.uri, "name": item.name, "description": item.description }))
                    .collect();
                rpc_result(id, json!({ "resources": resources }))
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
