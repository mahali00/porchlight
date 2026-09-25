use crate::config::{
    COMMAND_KEYS, Command, CommandApp, CommandObject, GROUP_SETTINGS, Input, InputType, McpServer, Settings, Words,
    decode_command, suggest,
};
use crate::core::{Problem, exposed_name, is_tool_name, is_usable_name};
use crate::links::LinkPlan;
use crate::policy::{Rules, rules_from};
use crate::sources::commands::{CommandTool, Lookup, LookupKind, Part, Word, literal, parse_word, sole_input};
use crate::sources::{CommandContribution, SourceSpec};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct AppSpec {
    pub name: String,
    pub source: SourceSpec,
    pub allow_dangerous: bool,
}

pub type ReadFile = Box<dyn Fn(&Path) -> Option<String> + Send + Sync>;

pub struct Environment {
    pub variables: HashMap<String, String>,
    pub path: String,
    pub read_file: ReadFile,
}

pub fn host_environment() -> Environment {
    Environment {
        variables: std::env::vars().collect(),
        path: std::env::var("PATH").unwrap_or_default(),
        read_file: Box::new(|path| std::fs::read_to_string(path).ok()),
    }
}

pub struct Apps {
    pub specs: Vec<AppSpec>,
    pub rules: Rules,
    pub problems: Vec<Problem>,
    pub links: Vec<LinkPlan>,
}

pub fn slugify(value: &str) -> String {
    let mut slug = String::new();

    for c in value.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }

    let trimmed = slug.trim_matches('-');

    if trimmed.is_empty() { "server".to_owned() } else { trimmed.to_owned() }
}

pub fn expand_home(word: &str) -> String {
    let home = || std::env::home_dir().unwrap_or_default();

    if word == "~" {
        return home().display().to_string();
    }

    match word.strip_prefix("~/") {
        Some(rest) => home().join(rest).display().to_string(),
        None => word.to_owned(),
    }
}

pub fn command_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut started = false;

    for c in command.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            started = true;
        } else if c == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
        } else if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                words.push(expand_home(&word));
            }

            word.clear();
            started = false;
        } else {
            word.push(c);
            started = true;
        }
    }

    if escaped {
        word.push('\\');
    }

    if started {
        words.push(expand_home(&word));
    }

    words
}

fn split_run(run: &Words) -> Vec<String> {
    match run {
        Words::One(text) => command_words(text),
        Words::Many(words) => match words.as_slice() {
            [only] if only.trim().contains(char::is_whitespace) && !Path::new(only).exists() => command_words(only),
            _ => words.iter().map(|word| expand_home(word)).collect(),
        },
    }
}

fn lookup_in(environment: &Environment) -> impl Fn(LookupKind, &str) -> Option<String> + '_ {
    move |kind, name| match kind {
        LookupKind::Env => environment.variables.get(name).cloned(),
        LookupKind::File => (environment.read_file)(Path::new(&expand_home(name))).map(|text| text.trim().to_owned()),
    }
}

fn expand_value(value: &str, lookup: Lookup) -> Result<String, String> {
    parse_word(value, lookup).map(|word| literal(&word))
}

fn expand_values(values: &IndexMap<String, String>, lookup: Lookup) -> Result<IndexMap<String, String>, String> {
    values.iter().map(|(name, value)| Ok((name.clone(), expand_value(value, lookup)?))).collect()
}

fn find_program(name: &str, environment: &Environment) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();

    which::which_in(name, Some(&environment.path), cwd).ok()
}

fn resolve_program(words: Vec<Word>, environment: &Environment) -> Result<Vec<Word>, String> {
    let mut words = words.into_iter();
    let program = words.next().ok_or("the command is empty")?;

    if program.iter().any(|part| matches!(part, Part::Input(_))) {
        return Ok(std::iter::once(program).chain(words).collect());
    }

    let name = literal(&program);
    let found = find_program(&name, environment).ok_or_else(|| format!("can't find a program called \"{name}\""))?;

    Ok(std::iter::once(vec![Part::Text(found.display().to_string())]).chain(words).collect())
}

fn parse_run(run: &[String], lookup: Lookup) -> Result<Vec<Word>, String> {
    run.iter().map(|word| parse_word(word, lookup)).collect()
}

#[derive(Default)]
struct Inherited {
    environment: IndexMap<String, String>,
    cwd: Option<String>,
    allow_dangerous: bool,
}

fn input_problem(words: &[Word], inputs: &IndexMap<String, Input>) -> Option<String> {
    for (name, input) in inputs {
        let numeric = matches!(input.kind, Some(InputType::Integer | InputType::Number));
        let position = words.iter().position(|word| sole_input(word) == Some(name.as_str()));

        if input.flag.is_some() && (input.kind != Some(InputType::Boolean) || position.is_none()) {
            return Some(format!("{{{name}}}: a flag needs type \"boolean\" and a word of its own"));
        }

        if input.choices.is_some() && input.kind.is_some_and(|kind| kind != InputType::String) {
            return Some(format!("{{{name}}}: choices only work for text inputs"));
        }

        if (input.min.is_some() || input.max.is_some()) && !numeric {
            return Some(format!("{{{name}}}: min and max need type \"integer\" or \"number\""));
        }

        let before = position.and_then(|at| at.checked_sub(1)).and_then(|at| words.get(at));

        if let Some(before) = before
            && input.is_optional()
            && sole_input(before).is_none()
            && literal(before).starts_with('-')
        {
            let option = literal(before);
            return Some(format!(
                "{{{name}}}: an optional input can't follow {option}, write it as {option}={{{name}}}"
            ));
        }
    }

    None
}

struct Compiled {
    tool: CommandTool,
    allow_dangerous: bool,
}

fn compile_command(command: &Command, inherited: &Inherited, environment: &Environment) -> Result<Compiled, String> {
    let text_settings;
    let settings = match command {
        Command::Text(run) => {
            text_settings = CommandObject {
                run: Words::One(run.clone()),
                description: None,
                read_only: None,
                resource: None,
                inputs: None,
                environment: None,
                cwd: None,
                allow_dangerous: None,
            };
            &text_settings
        }
        Command::Object(object) => object,
    };
    let run = split_run(&settings.run);
    let lookup = lookup_in(environment);
    let words = resolve_program(parse_run(&run, &lookup)?, environment)?;
    let resource = settings.resource == Some(true);

    if resource && words.iter().flatten().any(|part| matches!(part, Part::Input(_))) {
        return Err("a resource can't take inputs".to_owned());
    }

    let inputs = settings.inputs.clone().unwrap_or_default();

    if let Some(problem) = input_problem(&words, &inputs) {
        return Err(problem);
    }

    let mut merged = inherited.environment.clone();
    merged.extend(settings.environment.clone().unwrap_or_default());
    let variables = expand_values(&merged, &lookup)?;
    let cwd = settings.cwd.clone().or_else(|| inherited.cwd.clone());

    Ok(Compiled {
        tool: CommandTool {
            words,
            template: run.join(" "),
            description: settings.description.clone(),
            read_only: settings.read_only == Some(true) || resource,
            resource,
            inputs,
            environment: variables,
            cwd: cwd.map(|cwd| expand_home(&cwd)),
        },
        allow_dangerous: settings.allow_dangerous.unwrap_or(inherited.allow_dangerous),
    })
}

fn is_dangerous_app(app: &CommandApp) -> bool {
    match app {
        CommandApp::Single(Command::Text(_)) => false,
        CommandApp::Single(Command::Object(object)) => object.allow_dangerous == Some(true),
        CommandApp::Group(group) => {
            group.allow_dangerous == Some(true)
                || group
                    .members
                    .values()
                    .filter_map(decode_command)
                    .any(|member| matches!(member, Command::Object(object) if object.allow_dangerous == Some(true)))
        }
    }
}

struct CompiledApps {
    specs: Vec<AppSpec>,
    problems: Vec<Problem>,
}

fn problem(app: &str, message: String) -> Problem {
    Problem { app: app.to_owned(), message }
}

fn command_app(name: &str, app: &CommandApp, environment: &Environment) -> CompiledApps {
    let members: Vec<(String, Option<Command>)> = match app {
        CommandApp::Single(command) => vec![(name.to_owned(), Some(command.clone()))],
        CommandApp::Group(group) => group
            .members
            .iter()
            .filter(|(key, _)| !GROUP_SETTINGS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), decode_command(value)))
            .collect(),
    };
    let inherited = match app {
        CommandApp::Single(_) => Inherited::default(),
        CommandApp::Group(group) => Inherited {
            environment: group.environment.clone().unwrap_or_default(),
            cwd: group.cwd.clone(),
            allow_dangerous: group.allow_dangerous == Some(true),
        },
    };
    let mut problems = Vec::new();
    let mut compiled = Vec::new();

    for (tool, member) in members {
        let place = if tool == name { name.to_owned() } else { format!("{name}/{tool}") };

        match member {
            Some(command) if !COMMAND_KEYS.contains(&tool.as_str()) => {
                if !is_tool_name(&tool) {
                    problems.push(problem(name, format!("{place}: tool names use letters, numbers, _ and - only")));
                    continue;
                }

                match compile_command(&command, &inherited, environment) {
                    Ok(success) => compiled.push((tool, success)),
                    Err(failure) => problems.push(problem(name, format!("{place}: {failure}"))),
                }
            }
            _ => problems.push(problem(name, format!("{place} isn't a command{}", suggest(&tool, &COMMAND_KEYS)))),
        }
    }

    let allow_dangerous = is_dangerous_app(app) || compiled.iter().any(|(_, success)| success.allow_dangerous);
    let mut fitting = IndexMap::new();

    for (tool, success) in compiled {
        let exposed = exposed_name(name, &tool);

        if exposed.len() <= 64 {
            fitting.insert(tool, success.tool);
        } else {
            problems.push(problem(name, format!("{name}/{tool}: {exposed} is longer than 64 characters")));
        }
    }

    if fitting.is_empty() {
        return CompiledApps { specs: Vec::new(), problems };
    }

    let description = match app {
        CommandApp::Single(_) => None,
        CommandApp::Group(group) => group.description.clone(),
    };
    let source = SourceSpec::Commands {
        app: name.to_owned(),
        contribution: CommandContribution { tools: fitting, description },
    };

    CompiledApps { specs: vec![AppSpec { name: name.to_owned(), source, allow_dangerous }], problems }
}

fn mcp_source(name: &str, server: &McpServer, environment: &Environment) -> Result<SourceSpec, String> {
    let lookup = lookup_in(environment);

    match server {
        McpServer::Http(http) => Ok(SourceSpec::Http {
            url: expand_value(&http.url, &lookup)?,
            headers: expand_values(&http.headers.clone().unwrap_or_default(), &lookup)?,
        }),
        McpServer::Stdio(stdio) => {
            let words = resolve_program(parse_run(&split_run(&stdio.command), &lookup)?, environment)?;

            Ok(SourceSpec::Stdio {
                app: name.to_owned(),
                command: words.iter().map(literal).collect(),
                env: expand_values(&stdio.environment.clone().unwrap_or_default(), &lookup)?,
                cwd: stdio.cwd.as_deref().map(expand_home),
            })
        }
    }
}

fn mcp_app(name: &str, server: &McpServer, environment: &Environment) -> CompiledApps {
    if !server.enabled() {
        return CompiledApps { specs: Vec::new(), problems: Vec::new() };
    }

    match mcp_source(name, server, environment) {
        Ok(source) => CompiledApps {
            specs: vec![AppSpec { name: name.to_owned(), source, allow_dangerous: server.allow_dangerous() }],
            problems: Vec::new(),
        },
        Err(failure) => CompiledApps { specs: Vec::new(), problems: vec![problem(name, format!("{name}: {failure}"))] },
    }
}

fn compile_apps(settings: &Settings, environment: &Environment) -> CompiledApps {
    let mut seen = IndexSet::new();
    let mut specs = Vec::new();
    let mut problems = Vec::new();
    let named = settings
        .mcp
        .iter()
        .map(|(name, server)| (name, Some(server), None))
        .chain(settings.commands.iter().map(|(name, app)| (name, None, Some(app))));

    for (name, server, app) in named {
        let built = if !is_usable_name(name) {
            let message = format!("\"{name}\" can't be an app name. Use lowercase letters, numbers and -");
            CompiledApps { specs: Vec::new(), problems: vec![problem(name, message)] }
        } else if !seen.insert(name.clone()) {
            let message = format!("{name} is defined twice. Keep it in either mcp or commands");
            CompiledApps { specs: Vec::new(), problems: vec![problem(name, message)] }
        } else {
            match (server, app) {
                (Some(server), _) => mcp_app(name, server, environment),
                (None, Some(app)) => command_app(name, app, environment),
                (None, None) => CompiledApps { specs: Vec::new(), problems: Vec::new() },
            }
        };
        specs.extend(built.specs);
        problems.extend(built.problems);
    }

    CompiledApps { specs, problems }
}

fn compile_links(
    settings: &Settings,
    specs: &[AppSpec],
    configured: &IndexSet<String>,
) -> (Vec<LinkPlan>, Vec<Problem>) {
    let mut problems = Vec::new();
    let command_names: IndexSet<&str> =
        specs.iter().filter(|spec| spec.source.contribution().is_some()).map(|spec| spec.name.as_str()).collect();
    let direct_names: IndexSet<&str> =
        specs.iter().filter(|spec| spec.source.contribution().is_none()).map(|spec| spec.name.as_str()).collect();
    let mut plans: Vec<LinkPlan> = direct_names
        .iter()
        .map(|name| LinkPlan::Direct { name: (*name).to_owned(), app: (*name).to_owned() })
        .collect();

    for (name, members) in &settings.links {
        if !is_usable_name(name) {
            problems
                .push(problem(name, format!("\"{name}\" can't be a link name. Use lowercase letters, numbers and -")));
            continue;
        }

        if direct_names.contains(name.as_str()) {
            problems.push(problem(name, format!("{name} is both an MCP app and a command link. Rename one of them")));
            continue;
        }

        let requested: Vec<String> = if members.is_all() {
            configured.iter().cloned().collect()
        } else {
            members.apps().iter().cloned().collect::<IndexSet<String>>().into_iter().collect()
        };

        for app in &requested {
            if settings.mcp.contains_key(app) {
                problems.push(problem(name, format!("{name}: {app} is an MCP app and keeps its own link")));
            } else if !configured.contains(app) {
                problems.push(problem(name, format!("{name}: {app} isn't a command app")));
            }
        }

        plans.push(LinkPlan::Commands {
            name: name.clone(),
            apps: requested.iter().filter(|app| command_names.contains(app.as_str())).cloned().collect(),
            membership: requested,
        });
    }

    (plans, problems)
}

pub fn apps_from(settings: &Settings, environment: &Environment, configured: Option<&IndexSet<String>>) -> Apps {
    let default_configured: IndexSet<String> = settings.commands.keys().cloned().collect();
    let configured = configured.unwrap_or(&default_configured);
    let compiled = compile_apps(settings, environment);
    let (links, link_problems) = compile_links(settings, &compiled.specs, configured);

    Apps {
        specs: compiled.specs,
        rules: rules_from(&settings.tools),
        problems: compiled.problems.into_iter().chain(link_problems).collect(),
        links,
    }
}

pub fn template_words(run: &[String]) -> Vec<Word> {
    let keep = |kind: LookupKind, name: &str| {
        Some(match kind {
            LookupKind::Env => format!("{{env:{name}}}"),
            LookupKind::File => format!("{{file:{name}}}"),
        })
    };

    run.iter().map(|word| parse_word(word, &keep).unwrap_or_else(|_| vec![Part::Text(word.clone())])).collect()
}
