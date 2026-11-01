use crate::config::{COMMAND_KEYS, CommandApp, Input, InputType, McpServer, Settings, suggest};
use crate::core::{Problem, exposed_name, is_tool_name, is_usable_name};
use crate::links::LinkPlan;
use crate::policy::{Rules, rules_from};
use crate::sources::commands::{CommandTool, Lookup, LookupKind, Part, Word, literal, parse_word, sole_input};
use crate::sources::{AppSpec, CommandContribution, SourceSpec};
use crate::spec::{CommandSpec, expand_home, members, split_run};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

fn compile_command(spec: &CommandSpec, environment: &Environment) -> Result<CommandTool, String> {
    let lookup = lookup_in(environment);
    let words = resolve_program(parse_run(&spec.run, &lookup)?, environment)?;

    if spec.resource && words.iter().flatten().any(|part| matches!(part, Part::Input(_))) {
        return Err("a resource can't take inputs".to_owned());
    }

    if let Some(effect) = spec.risk().effect.filter(|_| spec.resource) {
        return Err(format!("a resource runs whenever a client reads it, so it can't be a command that {effect}"));
    }

    if let Some(problem) = input_problem(&words, &spec.inputs) {
        return Err(problem);
    }

    Ok(CommandTool {
        words,
        template: spec.shown(),
        description: spec.description.clone(),
        read_only: spec.read_only || spec.resource,
        resource: spec.resource,
        inputs: spec.inputs.clone(),
        environment: expand_values(&spec.environment, &lookup)?,
        cwd: spec.cwd.as_deref().map(expand_home),
    })
}

struct CompiledApps {
    specs: Vec<AppSpec>,
    problems: Vec<Problem>,
}

fn problem(app: &str, message: String) -> Problem {
    Problem { app: app.to_owned(), message }
}

fn command_app(name: &str, app: &CommandApp, environment: &Environment) -> CompiledApps {
    let mut problems = Vec::new();
    let mut compiled = Vec::new();
    let mut allow_dangerous = matches!(app, CommandApp::Group(group) if group.allow_dangerous == Some(true));

    for (tool, spec) in members(name, app) {
        let place = if tool == name { name.to_owned() } else { format!("{name}/{tool}") };

        match spec {
            Some(spec) if !COMMAND_KEYS.contains(&tool.as_str()) => {
                allow_dangerous |= spec.allow_dangerous;

                if !is_tool_name(&tool) {
                    problems.push(problem(name, format!("{place}: tool names use letters, numbers, _ and - only")));
                    continue;
                }

                match compile_command(&spec, environment) {
                    Ok(command) => compiled.push((tool, command)),
                    Err(failure) => problems.push(problem(name, format!("{place}: {failure}"))),
                }
            }
            _ => problems.push(problem(name, format!("{place} isn't a command{}", suggest(&tool, &COMMAND_KEYS)))),
        }
    }

    let mut fitting = IndexMap::new();

    for (tool, command) in compiled {
        let exposed = exposed_name(name, &tool);

        if exposed.len() <= 64 {
            fitting.insert(tool, command);
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
