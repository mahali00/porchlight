use crate::apps::{Environment, apps_from, host_environment};
use crate::config::{CommandApp, Config, ConfigError, Settings};
use crate::control::reload_daemon;
use crate::core::{is_app_name, link_path, slugify};
use crate::sources::commands::run_tool;
use crate::spec::CommandSpec;
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
    let mut standalone = match group.members.get(tool) {
        Some(Value::String(run)) => Map::from_iter([("run".to_owned(), json!(run))]),
        Some(Value::Object(member)) if member.contains_key("run") => member.clone(),
        _ => return Err(ConfigError { message: format!("{app} has no tool called {tool}") }),
    };
    let mut environment: Map<String, Value> =
        group.environment.iter().flatten().map(|(key, value)| (key.clone(), json!(value))).collect();

    if let Some(Value::Object(own)) = standalone.get("environment") {
        environment.extend(own.clone());
    }

    if !environment.is_empty() {
        standalone.insert("environment".to_owned(), Value::Object(environment));
    }

    if let Some(cwd) = &group.cwd {
        standalone.entry("cwd").or_insert_with(|| json!(cwd));
    }

    if let Some(allow) = group.allow_dangerous {
        standalone.entry("allowDangerous").or_insert_with(|| json!(allow));
    }

    let base = slugify(&format!("{app}-{tool}"));
    let name = (1..1000)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|name| !settings.commands.contains_key(name) && !settings.mcp.contains_key(name))
        .unwrap_or(base);

    config.edit(&["commands", &name], Some(&Value::Object(standalone)))?;
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

pub fn write_command(config: &Config, spec: &CommandSpec) -> Result<(), ConfigError> {
    config.edit(&["commands", &spec.app], Some(&spec.to_value()))
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

pub fn import_name(server: &crate::discovery::DiscoveredServer, settings: &crate::config::Settings) -> String {
    let taken = |name: &str| settings.mcp.contains_key(name) || settings.commands.contains_key(name);
    let name = if is_app_name(&server.name) { server.name.clone() } else { crate::core::slugify(&server.name) };
    if taken(&name) { format!("{name}-{}", crate::core::slugify(&server.source)) } else { name }
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

pub async fn try_draft(spec: &CommandSpec) -> Tried {
    let mut settings = Settings::default();

    match crate::config::decode_command_app(&spec.to_value()) {
        Ok(command) => {
            settings.commands.insert(spec.app.clone(), command);
            try_tool(&settings, &host_environment(), &spec.app, &spec.app, &json!({})).await
        }
        Err(error) => Tried { ok: false, text: error },
    }
}

pub fn tools_of(settings: &Settings, app: &str) -> Vec<String> {
    crate::spec::tool_names(app, settings.commands.get(app))
}

pub fn values_from_pairs(settings: &Settings, app: &str, tool: &str, pairs: &[(String, String)]) -> Value {
    let inputs = crate::spec::lookup(settings, app, tool).map(|spec| spec.inputs).unwrap_or_default();
    let values: Map<String, Value> = pairs
        .iter()
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
            (name.clone(), parsed)
        })
        .collect();

    Value::Object(values)
}

pub struct Placement {
    pub link: String,
    pub address: Option<String>,
}

pub struct Shared {
    pub app: String,
    pub placements: Vec<Placement>,
    pub running: bool,
    pub created: Vec<String>,
    pub turned_off: Vec<String>,
    pub reapproval: bool,
    pub problem: Option<String>,
}

pub async fn shared(config: &Config, app: &str) -> Shared {
    let settings = config.load().map(|loaded| loaded.settings).unwrap_or_default();
    let links = crate::links::links_for_app(&apps_from(&settings, &host_environment(), None).links, app);
    let state = reload_daemon().await;
    let problem = state.as_ref().and_then(|state| {
        let problems: Vec<&str> = state
            .problems
            .iter()
            .filter(|problem| problem.app == app)
            .map(|problem| problem.message.as_str())
            .collect();
        let refused = state
            .servers
            .iter()
            .find(|server| server.name == app && server.state == crate::links::ServerState::Refused)
            .map(|server| server.detail.clone());
        if problems.is_empty() { refused } else { Some(problems.join("; ")) }
    });

    Shared {
        app: app.to_owned(),
        placements: links
            .into_iter()
            .map(|link| {
                let address = state.as_ref().map(|state| format!("{}{}", state.public_url, link_path(&link)));
                Placement { link, address }
            })
            .collect(),
        running: state.is_some(),
        created: Vec::new(),
        turned_off: Vec::new(),
        reapproval: false,
        problem,
    }
}

pub async fn add_command(
    config: &Config,
    settings: &Settings,
    spec: &CommandSpec,
    links: &[String],
    approved_links: &[String],
) -> Result<Shared, ConfigError> {
    if let Some(link) = links.iter().find(|link| !is_app_name(link) || settings.mcp.contains_key(*link)) {
        return Err(ConfigError { message: format!("{link} can't be a command-tool link") });
    }

    write_command(config, spec)?;
    set_links_of(config, &config.load()?.settings, &spec.app, links)?;

    Ok(Shared {
        created: links.iter().filter(|link| !settings.links.contains_key(*link)).cloned().collect(),
        reapproval: links.iter().any(|link| approved_links.contains(link)),
        ..shared(config, &spec.app).await
    })
}

pub async fn share_server(config: &Config, name: &str, server: &Value) -> Result<Shared, ConfigError> {
    add_mcp(config, name, server)?;
    let state = reload_daemon().await;
    let turned_off: Vec<String> = state
        .iter()
        .flat_map(|state| state.servers.iter())
        .filter(|view| view.name == name)
        .flat_map(|view| view.catalog.iter())
        .filter(|tool| tool.risk == crate::policy::Level::RunsCode && tool.on)
        .map(|tool| tool.tool.clone())
        .collect();

    if !turned_off.is_empty() {
        tools_off(config, name, &turned_off)?;
        reload_daemon().await;
    }

    Ok(Shared { turned_off, ..shared(config, name).await })
}

fn tool_value(settings: &Settings, app: &str, tool: &str) -> Option<Value> {
    match settings.commands.get(app)? {
        CommandApp::Single(command) => serde_json::to_value(command).ok(),
        CommandApp::Group(group) => group.members.get(tool).cloned(),
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
    object.insert("run".into(), crate::spec::run_value(words));

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
