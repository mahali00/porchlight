use crate::core::{Problem, is_usable_name};
use crate::policy::is_rule_pattern;
use indexmap::{IndexMap, IndexSet};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    pub message: String,
}

fn error(message: impl Into<String>) -> ConfigError {
    ConfigError { message: message.into() }
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("PORCHLIGHT_HOME")
        .map_or_else(|| std::env::home_dir().unwrap_or_default().join(".porchlight"), PathBuf::from)
}

pub fn in_state_dir(name: &str) -> PathBuf {
    state_dir().join(name)
}

pub fn log_dir() -> PathBuf {
    in_state_dir("logs")
}

pub fn config_path() -> PathBuf {
    in_state_dir("porchlight.json")
}

const SCHEMA_URL: &str = "https://raw.githubusercontent.com/mahali00/porchlight/main/porchlight.schema.json";

pub const SETTINGS_KEYS: [&str; 7] = ["$schema", "tunnel", "port", "mcp", "commands", "tools", "links"];

pub const MCP_KEYS: [&str; 7] = ["url", "headers", "command", "environment", "cwd", "enabled", "allowDangerous"];

pub const COMMAND_KEYS: [&str; 8] =
    ["run", "description", "readOnly", "resource", "inputs", "environment", "cwd", "allowDangerous"];

pub const INPUT_KEYS: [&str; 7] = ["type", "description", "choices", "min", "max", "optional", "flag"];

pub const GROUP_SETTINGS: [&str; 4] = ["description", "environment", "cwd", "allowDangerous"];

pub type Strings = IndexMap<String, String>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Words {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    String,
    Integer,
    Number,
    Boolean,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<InputType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
}

impl Input {
    pub fn is_optional(&self) -> bool {
        self.optional == Some(true)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandObject {
    pub run: Words,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<IndexMap<String, Input>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Strings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_dangerous: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Command {
    Text(String),
    Object(Box<CommandObject>),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Strings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_dangerous: Option<bool>,
    #[serde(flatten)]
    pub members: IndexMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CommandApp {
    Single(Command),
    Group(Group),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpServer {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Strings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_dangerous: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StdioServer {
    pub command: Words,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Strings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_dangerous: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum McpServer {
    Http(HttpServer),
    Stdio(StdioServer),
}

impl McpServer {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Http(server) => server.enabled != Some(false),
            Self::Stdio(server) => server.enabled != Some(false),
        }
    }

    pub fn allow_dangerous(&self) -> bool {
        match self {
            Self::Http(server) => server.allow_dangerous == Some(true),
            Self::Stdio(server) => server.allow_dangerous == Some(true),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum LinkMembers {
    All(String),
    Apps(Vec<String>),
}

impl LinkMembers {
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All(_))
    }

    pub fn apps(&self) -> &[String] {
        match self {
            Self::All(_) => &[],
            Self::Apps(apps) => apps,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub mcp: IndexMap<String, McpServer>,
    pub commands: IndexMap<String, CommandApp>,
    pub tools: IndexMap<String, bool>,
    pub links: IndexMap<String, LinkMembers>,
}

#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub settings: Settings,
    pub problems: Vec<Problem>,
    pub links: IndexSet<String>,
    pub commands: IndexSet<String>,
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

fn check_input(input: &Input) -> Result<(), String> {
    match &input.flag {
        Some(flag) if !flag.starts_with('-') => Err(format!("a flag starts with -, not \"{flag}\"")),
        _ => Ok(()),
    }
}

pub fn decode_command_object(value: &Value) -> Result<CommandObject, String> {
    let command: CommandObject = decode(value)?;

    command.inputs.iter().flat_map(IndexMap::values).try_for_each(check_input)?;

    Ok(command)
}

pub fn decode_command(value: &Value) -> Option<Command> {
    match value {
        Value::String(text) => Some(Command::Text(text.clone())),
        Value::Object(_) => decode_command_object(value).ok().map(|command| Command::Object(Box::new(command))),
        _ => None,
    }
}

pub fn is_command(value: &Value) -> bool {
    decode_command(value).is_some()
}

fn is_group_member(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Bool(_)) || is_command(value) || decode::<Strings>(value).is_ok()
}

pub fn decode_command_app(value: &Value) -> Result<CommandApp, String> {
    if let Some(command) = decode_command(value) {
        return Ok(CommandApp::Single(command));
    }

    if value.as_object().is_some_and(|object| object.contains_key("run")) {
        return decode_command_object(value).map(|command| CommandApp::Single(Command::Object(Box::new(command))));
    }

    let group: Group = decode(value)?;

    match group.members.iter().find(|(_, member)| !is_group_member(member)) {
        Some((name, _)) => Err(format!("{name} isn't a command, a group setting or a list of settings")),
        None => Ok(CommandApp::Group(group)),
    }
}

pub fn decode_mcp_server(value: &Value) -> Result<McpServer, String> {
    match value.as_object() {
        Some(object) if object.contains_key("url") => decode(value).map(McpServer::Http),
        Some(object) if object.contains_key("command") => decode(value).map(McpServer::Stdio),
        Some(_) => Err("needs a url or a command".to_owned()),
        None => Err("needs an object with a url or a command".to_owned()),
    }
}

fn decode_link_members(value: &Value) -> Result<LinkMembers, String> {
    match value {
        Value::String(text) if text == "*" => Ok(LinkMembers::All(text.clone())),
        _ => decode(value).map(LinkMembers::Apps).map_err(|_| "a link lists app names, or \"*\"".to_owned()),
    }
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();

    for (row, left_char) in left.chars().enumerate() {
        let mut current = vec![row + 1];

        for (column, right_char) in right.iter().enumerate() {
            let above = previous.get(column + 1).copied().unwrap_or_default();
            let diagonal = previous.get(column).copied().unwrap_or_default();
            let beside = current.last().copied().unwrap_or_default();
            let substitution = diagonal + usize::from(left_char != *right_char);
            current.push((above + 1).min(beside + 1).min(substitution));
        }

        previous = current;
    }

    previous.last().copied().unwrap_or_default()
}

pub fn suggest(key: &str, known: &[&str]) -> String {
    known
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(key) || levenshtein(candidate, key) <= 2)
        .map(|found| format!(", did you mean \"{found}\"?"))
        .unwrap_or_default()
}

fn unknown_keys(value: &Value, known: &[&str], place: &str) -> Vec<String> {
    value
        .as_object()
        .into_iter()
        .flat_map(Map::keys)
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| format!("Unknown key \"{key}\"{place}{}", suggest(key, known)))
        .collect()
}

fn entries(value: Option<&Value>) -> impl Iterator<Item = (&String, &Value)> {
    value.and_then(Value::as_object).into_iter().flat_map(Map::iter)
}

fn unknown_key_problems(parsed: &Value) -> Vec<String> {
    let top = unknown_keys(parsed, &SETTINGS_KEYS, "");
    let servers =
        entries(parsed.get("mcp")).flat_map(|(name, entry)| unknown_keys(entry, &MCP_KEYS, &format!(" in mcp.{name}")));
    let commands = entries(parsed.get("commands")).flat_map(|(name, entry)| {
        if is_command(entry) {
            return unknown_keys(entry, &COMMAND_KEYS, &format!(" in commands.{name}"));
        }

        entries(Some(entry))
            .filter(|(_, member)| is_command(member))
            .flat_map(|(tool, member)| unknown_keys(member, &COMMAND_KEYS, &format!(" in commands.{name}.{tool}")))
            .collect()
    });

    top.into_iter().chain(servers).chain(commands).collect()
}

fn parse_options() -> ParseOptions {
    ParseOptions::default()
}

fn parse(text: &str) -> Result<Value, ConfigError> {
    jsonc_parser::parse_to_serde_value::<Value>(text, &parse_options())
        .map_err(|cause| error(format!("porchlight.json isn't valid JSON: {cause}")))
}

struct Frame {
    tunnel: Option<String>,
    port: Option<u16>,
    tools: IndexMap<String, bool>,
    links: IndexMap<String, LinkMembers>,
}

fn optional<T: serde::de::DeserializeOwned>(parsed: &Value, key: &str) -> Result<Option<T>, String> {
    parsed.get(key).map(|value| decode(value).map_err(|cause| format!("{key}: {cause}"))).transpose()
}

fn check_record(parsed: &Value, key: &str) -> Result<(), String> {
    match parsed.get(key) {
        None | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err(format!("{key} should be an object of names")),
    }
}

fn frame(parsed: &Value) -> Result<Frame, String> {
    if !parsed.is_object() {
        return Err("the file should hold one object".to_owned());
    }

    let _: Option<String> = optional(parsed, "$schema")?;
    check_record(parsed, "mcp")?;
    check_record(parsed, "commands")?;

    let tools: IndexMap<String, bool> = optional(parsed, "tools")?.unwrap_or_default();

    if let Some(pattern) = tools.keys().find(|pattern| !is_rule_pattern(pattern)) {
        return Err(format!("tools: \"{pattern}\" should look like app/tool"));
    }

    let links = entries(parsed.get("links"))
        .map(|(name, value)| decode_link_members(value).map(|members| (name.clone(), members)))
        .collect::<Result<IndexMap<_, _>, _>>()
        .map_err(|cause| format!("links: {cause}"))?;

    if parsed.get("links").is_some_and(|links| !links.is_object()) {
        return Err("links should be an object of link names".to_owned());
    }

    Ok(Frame { tunnel: optional(parsed, "tunnel")?, port: optional(parsed, "port")?, tools, links })
}

fn secure(path: &Path) -> Result<(), ConfigError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|cause| error(format!("Couldn't secure {}: {cause}", path.display())))
}

fn read_text(path: &Path) -> Result<String, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(cause) => Err(error(format!("Couldn't read {}: {cause}", path.display()))),
    }
}

fn decode_each<T>(
    section: &str,
    value: Option<&Value>,
    decoder: impl Fn(&Value) -> Result<T, String>,
    problems: &mut Vec<Problem>,
) -> IndexMap<String, T> {
    entries(value)
        .filter_map(|(name, entry)| match decoder(entry) {
            Ok(parsed) => Some((name.clone(), parsed)),
            Err(cause) => {
                problems.push(Problem { app: name.clone(), message: format!("{section}.{name}: {cause}") });
                None
            }
        })
        .collect()
}

pub fn load_text(text: &str) -> Result<(Loaded, bool), ConfigError> {
    if text.trim().is_empty() {
        return Ok((Loaded::default(), false));
    }

    let parsed = parse(text)?;
    let framed = frame(&parsed).map_err(|cause| error(format!("porchlight.json: {cause}")))?;
    let mut problems: Vec<Problem> =
        unknown_key_problems(&parsed).into_iter().map(|message| Problem { app: String::new(), message }).collect();
    let mcp = decode_each("mcp", parsed.get("mcp"), decode_mcp_server, &mut problems);
    let commands = decode_each("commands", parsed.get("commands"), decode_command_app, &mut problems);
    let links = entries(parsed.get("mcp"))
        .map(|(name, _)| name)
        .chain(framed.links.keys())
        .filter(|name| is_usable_name(name))
        .cloned()
        .collect();
    let command_names = entries(parsed.get("commands")).map(|(name, _)| name.clone()).collect();
    let settings =
        Settings { tunnel: framed.tunnel, port: framed.port, mcp, commands, tools: framed.tools, links: framed.links };

    Ok((Loaded { settings, problems, links, commands: command_names }, true))
}

fn to_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(flag) => CstInputValue::Bool(*flag),
        Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => CstInputValue::from(integer),
            (None, Some(float)) => CstInputValue::from(float),
            (None, None) => CstInputValue::Null,
        },
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        Value::Object(object) => {
            CstInputValue::Object(object.iter().map(|(key, item)| (key.clone(), to_input(item))).collect())
        }
    }
}

fn object_at(root: &CstRootNode, path: &[&str]) -> Result<CstObject, String> {
    path.iter().try_fold(root.object_value_or_set(), |object, segment| match object.get(segment) {
        None => Ok(object.object_value_or_set(segment)),
        Some(prop) => prop.object_value().ok_or_else(|| format!("{segment} isn't an object")),
    })
}

fn edited(text: &str, at: &[&str], value: Option<&Value>) -> Result<String, String> {
    let root = CstRootNode::parse(text, &parse_options()).map_err(|cause| cause.to_string())?;
    let (last, parents) = at.split_last().ok_or("nothing to change")?;
    let object = object_at(&root, parents)?;

    match (object.get(last), value) {
        (Some(existing), Some(value)) => existing.set_value(to_input(value)),
        (Some(existing), None) => existing.remove(),
        (None, Some(value)) => {
            object.append(last, to_input(value));
        }
        (None, None) => {}
    }

    Ok(root.to_string())
}

fn write(path: &Path, text: &str) -> Result<(), ConfigError> {
    let failed = |cause: std::io::Error| error(format!("Couldn't write {}: {cause}", path.display()));
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(failed)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, text).map_err(failed)?;
    secure(&temporary)?;
    fs::rename(&temporary, path).map_err(failed)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub path: PathBuf,
}

impl Config {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_location() -> Self {
        Self::at(config_path())
    }

    pub fn load(&self) -> Result<Loaded, ConfigError> {
        let (loaded, exists) = load_text(&read_text(&self.path)?)?;

        if exists {
            secure(&self.path)?;
        }

        Ok(loaded)
    }

    pub fn edit(&self, at: &[&str], value: Option<&Value>) -> Result<(), ConfigError> {
        let current = read_text(&self.path)?;
        let text =
            if current.trim().is_empty() { format!("{{\n  \"$schema\": \"{SCHEMA_URL}\"\n}}\n") } else { current };
        parse(&text)?;
        let place = at.join(".");
        let next = edited(&text, at, value)
            .map_err(|cause| error(format!("Couldn't change {place} in porchlight.json: {cause}")))?;
        let reparsed = parse(&next)?;
        frame(&reparsed)
            .map_err(|cause| error(format!("That change would break porchlight.json at {place}: {cause}")))?;

        write(&self.path, &next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_schema_lists_every_setting() {
        let schema: Value = serde_json::from_str(include_str!("../porchlight.schema.json")).unwrap();
        let keys =
            |value: &Value| -> Vec<String> { value["properties"].as_object().unwrap().keys().cloned().collect() };
        let mcp = &schema["properties"]["mcp"]["additionalProperties"]["anyOf"];
        let mut server_keys: Vec<String> = [keys(&mcp[0]), keys(&mcp[1])].concat();
        server_keys.sort();
        server_keys.dedup();
        let mut expected_mcp: Vec<String> = MCP_KEYS.iter().map(|key| (*key).to_owned()).collect();
        expected_mcp.sort();
        let command = &schema["properties"]["commands"]["additionalProperties"]["anyOf"][1];

        assert_eq!(keys(&schema), SETTINGS_KEYS);
        assert_eq!(server_keys, expected_mcp);
        assert_eq!(keys(command), COMMAND_KEYS);
        assert_eq!(keys(&command["properties"]["inputs"]["additionalProperties"]), INPUT_KEYS);
    }
}
