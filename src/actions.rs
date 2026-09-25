use crate::apps::{Environment, apps_from, host_environment, slugify, template_words};
use crate::config::{CommandApp, Config, ConfigError, Settings, decode_command};
use crate::control::reload_daemon;
use crate::core::{is_app_name, link_path};
use crate::sources::commands::{inputs_of, run_tool, runs_client_code};
use serde_json::{Map, Value, json};
use std::path::Path;

pub fn set_tool(config: &Config, rule: &str, on: bool) -> Result<(), ConfigError> {
    config.edit(&["tools", rule], Some(&json!(on)))
}

pub fn set_enabled(config: &Config, app: &str, on: bool) -> Result<(), ConfigError> {
    config.edit(&["mcp", app, "enabled"], Some(&json!(on)))
}

pub fn remove_app(config: &Config, settings: &Settings, app: &str) -> Result<(), ConfigError> {
    let section = if settings.commands.contains_key(app) { "commands" } else { "mcp" };
    config.edit(&[section, app], None)?;

    for (link, members) in &settings.links {
        if !members.is_all() && members.apps().iter().any(|member| member == app) {
            let kept: Vec<&String> = members.apps().iter().filter(|member| *member != app).collect();
            config.edit(&["links", link], Some(&json!(kept)))?;
        }
    }

    Ok(())
}

pub fn remove_link(config: &Config, link: &str) -> Result<(), ConfigError> {
    config.edit(&["links", link], None)
}

pub fn set_members(config: &Config, link: &str, picked: &[String]) -> Result<(), ConfigError> {
    if picked.is_empty() {
        return config.edit(&["links", link], None);
    }

    config.edit(&["links", link], Some(&json!(picked)))
}

pub fn new_link(config: &Config, name: &str, apps: &[String]) -> Result<(), ConfigError> {
    config.edit(&["links", name], Some(&json!(apps)))
}

pub fn set_links_of(config: &Config, settings: &Settings, app: &str, wanted: &[String]) -> Result<(), ConfigError> {
    for (link, members) in &settings.links {
        if members.is_all() {
            continue;
        }

        let has = members.apps().iter().any(|member| member == app);
        let want = wanted.contains(link);

        if has != want {
            let mut kept: Vec<String> = members.apps().iter().filter(|member| *member != app).cloned().collect();

            if want {
                kept.push(app.to_owned());
            }

            config.edit(&["links", link], Some(&json!(kept)))?;
        }
    }

    for link in wanted.iter().filter(|link| !settings.links.contains_key(*link)) {
        config.edit(&["links", link], Some(&json!([app])))?;
    }

    Ok(())
}

pub fn allow_dangerous(config: &Config, settings: &Settings, app: &str) -> Result<(), ConfigError> {
    match settings.commands.get(app) {
        None => config.edit(&["mcp", app, "allowDangerous"], Some(&json!(true))),
        Some(CommandApp::Single(crate::config::Command::Text(run))) => {
            config.edit(&["commands", app], Some(&json!({ "run": run, "allowDangerous": true })))
        }
        Some(_) => config.edit(&["commands", app, "allowDangerous"], Some(&json!(true))),
    }
}

pub fn tools_off(config: &Config, app: &str, tools: &[String]) -> Result<(), ConfigError> {
    tools.iter().try_for_each(|tool| set_tool(config, &format!("{app}/{tool}"), false))
}

pub fn remove_tool(config: &Config, settings: &Settings, app: &str, tool: &str) -> Result<(), ConfigError> {
    if tools_of(settings, app).len() <= 1 {
        return remove_app(config, settings, app);
    }

    config.edit(&["commands", app, tool], None)
}

pub fn split_tool(config: &Config, settings: &Settings, app: &str, tool: &str) -> Result<String, ConfigError> {
    let Some(CommandApp::Group(group)) = settings.commands.get(app) else {
        return Err(ConfigError { message: format!("{app} has only one tool") });
    };
    let Some(member) = group.members.get(tool) else {
        return Err(ConfigError { message: format!("{app} has no tool called {tool}") });
    };
    let base = slugify(&format!("{app}-{tool}"));
    let name = (1..1000)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|name| !settings.commands.contains_key(name) && !settings.mcp.contains_key(name))
        .unwrap_or(base);

    config.edit(&["commands", &name], Some(member))?;
    config.edit(&["commands", app, tool], None)?;
    let links: Vec<String> = settings
        .links
        .iter()
        .filter(|(_, members)| members.apps().iter().any(|member| member == app))
        .map(|(link, _)| link.clone())
        .collect();
    let latest = config.load()?.settings;
    set_links_of(config, &latest, &name, &links)?;

    Ok(name)
}

pub fn deny(store: &crate::store::Store, request: &str) -> Result<(), crate::store::StoreError> {
    store.pending_remove(request)?;
    store.audit_write("authz.denied", request)
}

pub fn set_tunnel(config: &Config, tunnel: &str) -> Result<(), ConfigError> {
    config.edit(&["tunnel"], Some(&json!(tunnel)))
}

pub fn run_value(words: &[String]) -> Value {
    if words.iter().all(|word| !word.is_empty() && !word.contains(|c: char| c.is_whitespace() || c == '"' || c == '\''))
    {
        json!(words.join(" "))
    } else {
        json!(words)
    }
}

pub struct CommandDraft {
    pub words: Vec<String>,
    pub app: String,
    pub description: Option<String>,
    pub read_only: bool,
    pub allow_dangerous: bool,
}

impl CommandDraft {
    pub fn program(&self) -> String {
        self.words.first().cloned().unwrap_or_default()
    }

    pub fn shown(&self) -> String {
        self.words.join(" ")
    }

    pub fn takes_inputs(&self) -> bool {
        !inputs_of(&template_words(&self.words)).is_empty()
    }

    pub fn runs_client_code(&self) -> bool {
        runs_client_code(&template_words(&self.words))
    }

    pub fn default_name(&self) -> String {
        slugify(Path::new(&self.program()).file_name().and_then(|name| name.to_str()).unwrap_or("tool"))
    }

    fn settings(&self) -> Value {
        let run = run_value(&self.words);

        if run.is_string() && self.description.is_none() && !self.read_only && !self.allow_dangerous {
            return run;
        }

        let mut object = Map::new();
        object.insert("run".into(), run);

        if let Some(description) = &self.description {
            object.insert("description".into(), json!(description));
        }

        if self.read_only {
            object.insert("readOnly".into(), json!(true));
        }

        if self.allow_dangerous {
            object.insert("allowDangerous".into(), json!(true));
        }

        Value::Object(object)
    }
}

pub fn program_problem(program: &str) -> Option<String> {
    if program.is_empty() {
        return Some("Type the command to run".to_owned());
    }

    (!program.contains('/') && which::which(program).is_err())
        .then(|| format!("Can't find a program called \"{program}\". Check the spelling, or use its full path"))
}

pub fn name_problem(name: &str, settings: &Settings) -> Option<String> {
    if !is_app_name(name) {
        return Some(format!("\"{name}\" can't be a name. Use lowercase letters, numbers and -"));
    }

    (settings.mcp.contains_key(name) || settings.commands.contains_key(name))
        .then(|| format!("{name} already exists. Pick another name"))
}

pub fn write_command(config: &Config, draft: &CommandDraft) -> Result<(), ConfigError> {
    config.edit(&["commands", &draft.app], Some(&draft.settings()))
}

pub fn add_mcp(config: &Config, name: &str, server: &Value) -> Result<(), ConfigError> {
    config.edit(&["mcp", name], Some(server))
}

pub fn server_name(command: &[String]) -> String {
    let named = command
        .iter()
        .find(|word| word.to_lowercase().contains("mcp") || word.contains("modelcontextprotocol"))
        .or_else(|| command.first())
        .cloned()
        .unwrap_or_else(|| "server".to_owned());
    let without_version = match named.rfind('@') {
        Some(at) if at > 0 => named.get(..at).unwrap_or(&named).to_owned(),
        _ => named,
    };
    let base = Path::new(&without_version).file_name().and_then(|name| name.to_str()).unwrap_or("server").to_owned();
    let trimmed = base
        .trim_start_matches("mcp-server-")
        .trim_start_matches("server-")
        .trim_start_matches("mcp-")
        .trim_end_matches("-mcp-server")
        .trim_end_matches("-mcp");

    slugify(if trimmed.is_empty() { &base } else { trimmed })
}

pub struct Tried {
    pub ok: bool,
    pub text: String,
}

pub async fn try_tool(settings: &Settings, environment: &Environment, app: &str, tool: &str, values: &Value) -> Tried {
    let compiled = apps_from(settings, environment, None);

    if let Some(problem) = compiled.problems.iter().find(|problem| problem.app == app) {
        return Tried { ok: false, text: problem.message.clone() };
    }

    let command = compiled
        .specs
        .iter()
        .find(|spec| spec.name == app)
        .and_then(|spec| spec.source.contribution())
        .and_then(|contribution| contribution.tools.get(tool));
    let Some(command) = command else {
        return Tried { ok: false, text: format!("{app} has no tool called {tool}") };
    };
    let run = run_tool(command, values).await;

    Tried { ok: !run.is_error, text: run.text }
}

pub async fn try_draft(draft: &CommandDraft) -> Tried {
    let mut settings = Settings::default();

    match crate::config::decode_command_app(&draft.settings()) {
        Ok(command) => {
            settings.commands.insert(draft.app.clone(), command);
            try_tool(&settings, &host_environment(), &draft.app, &draft.app, &json!({})).await
        }
        Err(error) => Tried { ok: false, text: error },
    }
}

pub fn tools_of(settings: &Settings, app: &str) -> Vec<String> {
    crate::snapshot::tool_names(app, settings.commands.get(app))
}

pub fn parse_values(settings: &Settings, app: &str, tool: &str, text: &str) -> Value {
    let inputs = match settings.commands.get(app) {
        Some(CommandApp::Single(crate::config::Command::Object(object))) => object.inputs.clone(),
        Some(CommandApp::Group(group)) => {
            group.members.get(tool).and_then(decode_command).and_then(|command| match command {
                crate::config::Command::Object(object) => object.inputs,
                crate::config::Command::Text(_) => None,
            })
        }
        _ => None,
    }
    .unwrap_or_default();
    let values: Map<String, Value> = text
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| {
            let kind = inputs.get(name).and_then(|input| input.kind);
            let parsed = match kind {
                Some(crate::config::InputType::Boolean) => json!(value == "true"),
                Some(crate::config::InputType::Integer | crate::config::InputType::Number) => value
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
                    .map_or_else(|| json!(value), |number| json!(number)),
                _ => json!(value),
            };
            (name.to_owned(), parsed)
        })
        .collect();

    Value::Object(values)
}

pub async fn verdict(config: &Config, app: &str) -> (bool, Vec<String>) {
    let settings = config.load().map(|loaded| loaded.settings).unwrap_or_default();
    let links = apps_from(&settings, &host_environment(), None).links;
    let names = crate::links::links_for_app(&links, app);
    let Some(state) = reload_daemon().await else {
        return (
            true,
            vec![if names.is_empty() {
                format!("Saved {app}. Share it on a link in the Links tab.")
            } else {
                format!("Saved {app} on {}. It goes live when porchlight starts.", names.join(", "))
            }],
        );
    };
    let problems: Vec<String> =
        state.problems.iter().filter(|problem| problem.app == app).map(|problem| problem.message.clone()).collect();
    let refused =
        state.servers.iter().find(|server| server.name == app && server.state == crate::links::ServerState::Refused);

    if !problems.is_empty() || refused.is_some() {
        let reason = if problems.is_empty() {
            refused.map(|server| server.detail.clone()).unwrap_or_default()
        } else {
            problems.join("; ")
        };
        return (false, vec![format!("Couldn't share {app}: {reason}")]);
    }

    if names.is_empty() {
        return (true, vec![format!("Saved {app}. Share it on a link in the Links tab.")]);
    }

    (
        true,
        names
            .iter()
            .map(|link| {
                format!("Add {}{} in your MCP client, then approve it here.", state.public_url, link_path(link))
            })
            .collect(),
    )
}

pub async fn share_mcp(config: &Config, name: &str, server: &Value) -> Vec<String> {
    if let Err(error) = add_mcp(config, name, server) {
        return vec![format!("✗ {name}: {}", error.message)];
    }

    let state = reload_daemon().await;
    let risky: Vec<String> = state
        .iter()
        .flat_map(|state| state.servers.iter())
        .filter(|view| view.name == name)
        .flat_map(|view| view.catalog.iter())
        .filter(|tool| tool.dangerous && tool.on)
        .map(|tool| tool.tool.clone())
        .collect();

    if !risky.is_empty() {
        let _ = tools_off(config, name, &risky);
        reload_daemon().await;
    }

    let (_, mut lines) = verdict(config, name).await;

    if !risky.is_empty() {
        lines.push(format!("▲ {} can run code, so they're off. Allow them from the Links tab (D).", risky.join(", ")));
    }

    lines
}

pub struct ToolSetup {
    pub run: String,
    pub description: Option<String>,
    pub read_only: bool,
}

fn tool_value(settings: &Settings, app: &str, tool: &str) -> Option<Value> {
    match settings.commands.get(app)? {
        CommandApp::Single(command) => serde_json::to_value(command).ok(),
        CommandApp::Group(group) => group.members.get(tool).cloned(),
    }
}

pub fn tool_setup(settings: &Settings, app: &str, tool: &str) -> Option<ToolSetup> {
    let words = |run: &crate::config::Words| match run {
        crate::config::Words::One(text) => text.clone(),
        crate::config::Words::Many(words) => words.join(" "),
    };

    match decode_command(&tool_value(settings, app, tool)?)? {
        crate::config::Command::Text(run) => Some(ToolSetup { run, description: None, read_only: false }),
        crate::config::Command::Object(object) => Some(ToolSetup {
            run: words(&object.run),
            description: object.description.clone(),
            read_only: object.read_only == Some(true),
        }),
    }
}

pub fn edit_tool(
    config: &Config,
    settings: &Settings,
    app: &str,
    tool: &str,
    words: &[String],
    description: &str,
    read_only: bool,
) -> Result<(), ConfigError> {
    let missing = || ConfigError { message: format!("{app} has no tool called {tool}") };
    let mut object = match tool_value(settings, app, tool).ok_or_else(missing)? {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    object.insert("run".into(), run_value(words));

    if description.is_empty() {
        object.remove("description");
    } else {
        object.insert("description".into(), json!(description));
    }

    if read_only {
        object.insert("readOnly".into(), json!(true));
    } else {
        object.remove("readOnly");
    }

    let value = match object.get("run") {
        Some(Value::String(run)) if object.len() == 1 => json!(run),
        _ => Value::Object(object),
    };
    let path: Vec<&str> = if matches!(settings.commands.get(app), Some(CommandApp::Group(_))) {
        vec!["commands", app, tool]
    } else {
        vec!["commands", app]
    };

    config.edit(&path, Some(&value))
}
