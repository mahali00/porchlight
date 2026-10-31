use crate::config::{Command, CommandApp, GROUP_SETTINGS, Group, Input, Settings, Words, decode_command};
use crate::core::exposed_name;
use crate::policy::Risk;
use crate::sources::commands::{LookupKind, Part, Word, assess, inputs_of, parse_word};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandSpec {
    pub app: String,
    pub tool: String,
    pub run: Vec<String>,
    pub description: Option<String>,
    pub read_only: bool,
    pub resource: bool,
    pub inputs: IndexMap<String, Input>,
    pub environment: IndexMap<String, String>,
    pub cwd: Option<String>,
    pub allow_dangerous: bool,
}

impl CommandSpec {
    pub fn draft(app: &str, run: Vec<String>) -> Self {
        Self { app: app.to_owned(), tool: app.to_owned(), run, ..Self::default() }
    }

    pub fn template(&self) -> Vec<Word> {
        template_words(&self.run)
    }

    pub fn placeholders(&self) -> Vec<String> {
        inputs_of(&self.template())
    }

    pub fn risk(&self) -> Risk {
        assess(&self.template(), &self.inputs, self.read_only || self.resource)
    }

    #[must_use]
    pub fn honest(mut self) -> Self {
        if self.risk().contradiction().is_some() {
            self.read_only = false;
        }
        self
    }

    pub fn program(&self) -> &str {
        self.run.first().map_or("", String::as_str)
    }

    pub fn program_name(&self) -> &str {
        self.program().rsplit('/').next().unwrap_or_default()
    }

    pub fn shown(&self) -> String {
        self.run.join(" ")
    }

    pub fn default_name(&self) -> String {
        crate::core::slugify(Path::new(self.program()).file_name().and_then(|name| name.to_str()).unwrap_or("tool"))
    }

    pub fn exposed(&self) -> String {
        if self.tool == self.app { self.app.clone() } else { exposed_name(&self.app, &self.tool) }
    }

    pub fn to_value(&self) -> Value {
        let run = run_value(&self.run);
        let mut settings = Map::new();

        if let Some(description) = &self.description {
            settings.insert("description".into(), json!(description));
        }
        if self.read_only {
            settings.insert("readOnly".into(), json!(true));
        }
        if self.resource {
            settings.insert("resource".into(), json!(true));
        }
        if !self.inputs.is_empty() {
            settings.insert("inputs".into(), json!(self.inputs));
        }
        if !self.environment.is_empty() {
            settings.insert("environment".into(), json!(self.environment));
        }
        if let Some(cwd) = &self.cwd {
            settings.insert("cwd".into(), json!(cwd));
        }
        if self.allow_dangerous {
            settings.insert("allowDangerous".into(), json!(true));
        }

        if settings.is_empty() && run.is_string() {
            return run;
        }

        let mut object = Map::from_iter([("run".to_owned(), run)]);
        object.extend(settings);
        Value::Object(object)
    }
}

pub fn decode(app: &str, tool: &str, command: &Command, group: Option<&Group>) -> CommandSpec {
    let inherited = group.cloned().unwrap_or_default();
    let mut environment = inherited.environment.unwrap_or_default();

    match command {
        Command::Text(run) => CommandSpec {
            app: app.to_owned(),
            tool: tool.to_owned(),
            run: command_words(run),
            environment,
            cwd: inherited.cwd,
            allow_dangerous: inherited.allow_dangerous == Some(true),
            ..CommandSpec::default()
        },
        Command::Object(object) => {
            environment.extend(object.environment.clone().unwrap_or_default());
            CommandSpec {
                app: app.to_owned(),
                tool: tool.to_owned(),
                run: split_run(&object.run),
                description: object.description.clone(),
                read_only: object.read_only == Some(true),
                resource: object.resource == Some(true),
                inputs: object.inputs.clone().unwrap_or_default(),
                environment,
                cwd: object.cwd.clone().or(inherited.cwd),
                allow_dangerous: object.allow_dangerous.unwrap_or(inherited.allow_dangerous == Some(true)),
            }
        }
    }
}

pub fn tool_names(app: &str, command: Option<&CommandApp>) -> Vec<String> {
    match command {
        None => Vec::new(),
        Some(CommandApp::Single(_)) => vec![app.to_owned()],
        Some(CommandApp::Group(group)) => {
            group.members.keys().filter(|key| !GROUP_SETTINGS.contains(&key.as_str())).cloned().collect()
        }
    }
}

pub fn members(app: &str, command: &CommandApp) -> Vec<(String, Option<CommandSpec>)> {
    match command {
        CommandApp::Single(single) => vec![(app.to_owned(), Some(decode(app, app, single, None)))],
        CommandApp::Group(group) => tool_names(app, Some(command))
            .into_iter()
            .map(|tool| {
                let spec = group
                    .members
                    .get(&tool)
                    .and_then(decode_command)
                    .map(|member| decode(app, &tool, &member, Some(group)));
                (tool, spec)
            })
            .collect(),
    }
}

pub fn lookup(settings: &Settings, app: &str, tool: &str) -> Option<CommandSpec> {
    members(app, settings.commands.get(app)?).into_iter().find(|(name, _)| name == tool).and_then(|(_, spec)| spec)
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

pub fn split_run(run: &Words) -> Vec<String> {
    match run {
        Words::One(text) => command_words(text),
        Words::Many(words) => match words.as_slice() {
            [only] if only.trim().contains(char::is_whitespace) && !Path::new(only).exists() => command_words(only),
            _ => words.iter().map(|word| expand_home(word)).collect(),
        },
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

pub fn run_value(words: &[String]) -> Value {
    if words.iter().all(|word| !word.is_empty() && !word.contains(|c: char| c.is_whitespace() || c == '"' || c == '\''))
    {
        json!(words.join(" "))
    } else {
        json!(words)
    }
}
