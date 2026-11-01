use super::{
    checked_name, confirm, failed, load, no_link, no_tool, pairs, resolve, saved, say, say_all, terminal_text,
};
use crate::actions;
use crate::apps::host_environment;
use crate::exit::{ExitCode, ExitError};
use crate::policy::Level;
use crate::snapshot::Snapshot;
use crate::spec::CommandSpec;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

pub(super) fn checked_command(spec: &CommandSpec, asked_before: bool, verb: &str) -> Result<Option<String>, ExitError> {
    let risk = spec.risk();
    if let Some(problem) = actions::program_problem(spec.program()) {
        return Err(ExitError::new(ExitCode::NoServer, problem));
    }
    let program = spec.program_name();
    if risk.level() == Level::RunsCode {
        return Err(ExitError::new(ExitCode::NoServer, "This command can run anything a client sends it")
            .next("Name the exact program instead, like backup.sh {folder}."));
    }
    if let Some(effect) = risk.contradiction() {
        return Err(ExitError::new(ExitCode::NoServer, format!("{program} {effect}, so it can't be read-only"))
            .next("Leave out --read-only. Clients are then told it can change things."));
    }
    if !risk.needs_person() || asked_before {
        let note = risk.effect.filter(|_| risk.level() == Level::Changes);
        return Ok(note.map(|effect| format!("▲ It {effect} each time a client calls it.")));
    }
    if !std::io::stdin().is_terminal() {
        return Err(ExitError::new(
            ExitCode::NeedsHuman,
            "A tool that can change things, with text a client chooses, needs a person at this computer",
        )
        .next("Run it yourself in a terminal. An agent or script can't do this."));
    }
    for (index, line) in crate::explain::caution(spec).iter().enumerate() {
        eprintln!("{}{line}", if index == 0 { "▲ " } else { "  " });
    }
    eprint!("{verb} it anyway? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|error| failed(&error))?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        return Err(ExitError::new(ExitCode::NoServer, "Nothing was changed"));
    }
    Ok(Some(format!("▲ {}. Clients are told it can change things.", risk.summary())))
}

pub(super) async fn edit_tool(
    target: &str,
    words: Vec<String>,
    description: Option<String>,
    read_only: Option<bool>,
    json: bool,
) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let (app, tool) = resolve(target, &snap)?;
    let tool = tool.unwrap_or_else(|| app.clone());
    let Some(current) = crate::spec::lookup(&snap.settings, &app, &tool) else {
        return Err(no_tool(target, &snap));
    };
    let asked_before = (words.is_empty() || words == current.run) && current.risk().needs_person();
    let edited = CommandSpec {
        run: if words.is_empty() { current.run.clone() } else { words },
        read_only: read_only.unwrap_or(current.read_only),
        description: description.or_else(|| current.description.clone()),
        ..current
    };
    let note = checked_command(&edited, asked_before, "Save")?;
    let about = edited.description.clone().unwrap_or_default();
    saved(actions::edit_tool(&config, &snap.settings, &app, &tool, &edited.run, &about, edited.read_only)).await?;
    say_all(json, &format!("Saved {target}"), &note.into_iter().collect::<Vec<_>>());
    Ok(())
}

pub(super) async fn add_command(
    name: String,
    words: Vec<String>,
    read_only: bool,
    description: Option<String>,
    mut links: Vec<String>,
    env: Vec<String>,
    json: bool,
) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let command = CommandSpec {
        description,
        read_only,
        environment: pairs(&env)?.into_iter().collect(),
        ..CommandSpec::draft(&name, words)
    };
    checked_name(&command.app, &snap)?;
    let note = checked_command(&command, false, "Add")?;
    let defaulted = links.is_empty();
    if defaulted {
        links.push("tools".to_owned());
    }
    let approved: Vec<String> =
        snap.links.iter().filter(|info| !info.clients.is_empty()).map(|info| info.name.clone()).collect();
    let mut shared = actions::add_command(&config, &snap.settings, &command, &links, &approved)
        .await
        .map_err(|error| ExitError::new(ExitCode::NoServer, error.message))?;
    if defaulted {
        shared.created.clear();
    }
    if let Some(problem) = &shared.problem {
        return Err(ExitError::new(ExitCode::ToolFailed, format!("Couldn't share {}: {problem}", shared.app)));
    }
    let mut messages = crate::explain::shared(&shared);
    messages.extend(note);
    if !shared.running {
        messages.push("Next: porchlight start".to_owned());
    }
    say_all(json, &format!("Added {}", command.app), &messages);
    Ok(())
}

pub(super) async fn remove_target(target: &str, force: bool, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let resolved = match resolve(target, &snap)? {
        (app, Some(tool)) => format!("{app}/{tool}"),
        (app, None) => app,
    };
    let target = resolved.as_str();
    if let Some((app, tool)) = target.split_once('/') {
        let Some(info) = snap.apps.iter().find(|info| info.name == app) else {
            return Err(no_tool(target, &snap));
        };
        if !info.tool_names.iter().any(|name| name == tool) {
            return Err(no_tool(target, &snap));
        }
        if !snap.settings.commands.contains_key(app) {
            return Err(ExitError::new(ExitCode::NoServer, format!("{target} belongs to the MCP server {app}"))
                .next(format!("Turn it off with porchlight tools disable {target}.")));
        }
        confirm(&format!("Remove tool {target}?"), force)?;
        saved(actions::remove_tool(&config, &snap.settings, app, tool)).await?;
    } else {
        if !snap.app_names().iter().any(|name| name == target) {
            return Err(no_tool(target, &snap));
        }
        confirm(&format!("Remove {target} and its tools?"), force)?;
        saved(actions::remove_app(&config, &snap.settings, target)).await?;
    }
    say(json, &format!("Removed {target}"));
    Ok(())
}

pub(super) struct ToolLine {
    name: String,
    on: bool,
    level: Level,
    summary: String,
    links: Vec<String>,
    inputs: Vec<String>,
}

impl ToolLine {
    fn shown(&self) -> String {
        std::iter::once(self.name.clone())
            .chain(self.inputs.iter().map(|input| format!("{{{input}}}")))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(super) fn links_with(snap: &Snapshot, rule: &str) -> Vec<String> {
    snap.links
        .iter()
        .filter(|item| item.groups.iter().any(|group| group.tools.iter().any(|tool| tool.rule() == rule)))
        .map(|item| item.name.clone())
        .collect()
}

pub(super) async fn tools_list(link: Option<&str>, json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;
    if let Some(link) = link
        && !snap.links.iter().any(|item| item.name == link)
    {
        return Err(no_link(link, &snap));
    }
    let mut tools: Vec<ToolLine> = Vec::new();
    let mut seen = Vec::new();
    for (app, _) in &snap.settings.commands {
        for name in actions::tools_of(&snap.settings, app) {
            let Some(spec) = crate::spec::lookup(&snap.settings, app, &name) else {
                continue;
            };
            let rule = format!("{app}/{name}");
            let links = links_with(&snap, &rule);
            if link.is_some_and(|link| !links.iter().any(|item| item == link)) {
                continue;
            }
            let risk = spec.risk();
            tools.push(ToolLine {
                name: spec.exposed(),
                on: crate::policy::tool_on(&snap.rules, app, &name),
                level: risk.level(),
                summary: risk.summary(),
                links,
                inputs: spec.placeholders(),
            });
            seen.push(rule);
        }
    }
    for item in snap.links.iter().filter(|item| link.is_none_or(|link| item.name == link)) {
        for group in item.groups.iter().filter(|group| snap.settings.mcp.contains_key(&group.name)) {
            for tool in &group.tools {
                let rule = tool.rule();
                if seen.contains(&rule) {
                    continue;
                }
                tools.push(ToolLine {
                    name: tool.exposed.clone(),
                    on: tool.on,
                    level: tool.level,
                    summary: tool.level.word().to_owned(),
                    links: links_with(&snap, &rule),
                    inputs: Vec::new(),
                });
                seen.push(rule);
            }
        }
    }
    if json {
        let listed: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name, "on": tool.on, "risk": tool.level, "summary": tool.summary,
                    "links": tool.links, "inputs": tool.inputs
                })
            })
            .collect();
        out!("{}", json!({ "ok": true, "tools": listed }));
        return Ok(());
    }
    let width = tools.iter().map(|tool| tool.shown().chars().count()).max().unwrap_or_default();
    let about = tools.iter().map(|tool| tool.summary.chars().count()).max().unwrap_or_default();
    for tool in &tools {
        out!(
            "{:<4}{:<width$}  {:<about$}  {}",
            if tool.on { "on" } else { "off" },
            tool.shown(),
            tool.summary,
            tool.links.join(", ")
        );
    }
    if tools.is_empty() {
        out!("No tools yet. Add one with porchlight tools add <name> -- <command>");
    }
    Ok(())
}

pub(super) async fn switch(target: &str, on: bool, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let word = if on { "enabled" } else { "disabled" };
    let (app, tool) = resolve(target, &snap)?;
    let (app, tool) = (app.as_str(), tool.as_deref());
    if !snap.app_names().iter().any(|name| name == app) {
        return Err(no_tool(target, &snap));
    }
    match tool {
        Some(tool) => {
            let valid = snap
                .apps
                .iter()
                .find(|info| info.name == app)
                .is_some_and(|info| info.tool_names.iter().any(|name| name == tool));
            if !valid {
                return Err(no_tool(target, &snap));
            }
            saved(actions::set_tool(&config, &format!("{app}/{tool}"), on)).await?;
        }
        None if snap.settings.mcp.contains_key(app) => saved(actions::set_enabled(&config, app, on)).await?,
        None => saved(actions::set_tool(&config, &format!("{app}/*"), on)).await?,
    }
    say(json, &format!("{target} is {word}"));
    Ok(())
}

pub(super) async fn try_tool(target: &str, inputs: &[String], json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;
    let (app, tool) = resolve(target, &snap)?;
    let tool = tool.unwrap_or_else(|| app.clone());
    let (app, tool) = (app.as_str(), tool.as_str());
    let pairs = pairs(inputs)?;
    let values = actions::values_from_pairs(&snap.settings, app, tool, &pairs);
    let (ok, text) = if snap.settings.commands.contains_key(app) {
        let tried = actions::try_tool(&snap.settings, &host_environment(), app, tool, &values).await;
        (tried.ok, tried.text)
    } else if snap.settings.mcp.contains_key(app) {
        crate::control::try_on_daemon(app, tool, &values)
            .await
            .map_err(|error| ExitError::new(ExitCode::ToolFailed, error))?
    } else {
        return Err(no_tool(target, &snap));
    };
    if !ok {
        let reason = terminal_text(&text);
        let reason = reason.trim();
        let message = if reason.is_empty() { format!("{target} failed") } else { format!("{target} failed: {reason}") };
        return Err(ExitError::new(ExitCode::ToolFailed, message));
    }
    if json {
        out!("{}", json!({ "ok": true, "output": text }));
    } else {
        out!("{}", terminal_text(&text).trim_end());
    }
    Ok(())
}
