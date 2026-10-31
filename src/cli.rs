use crate::actions;
use crate::config::{Config, ConfigError};
use crate::control::reload_daemon;
use crate::exit::{ExitCode, ExitError};
use crate::links::ServerState;
use crate::snapshot::{self, Snapshot, Verdict};
use crate::store::Store;
use clap::CommandFactory;
use serde_json::json;
use std::io::{IsTerminal, Write};
mod access;
mod args;
mod links;
mod servers;
mod status;
mod tools;

use access::{client_approve, client_deny, clients, clients_revoke, token_create, token_revoke, tokens_list};
pub use args::{
    Cli, ClientsAction, Command, LinksAction, LogsArgs, ServersAction, TokensAction, ToolsAction, TunnelAction,
};
use links::{link_add, link_delete, link_edit, links};
use servers::{servers_add, servers_import, servers_list};
use status::{logs, start, status, tunnel, use_tunnel};
use tools::{add_command, edit_tool, remove_target, switch, tools_list, try_tool};

fn completions(shell: clap_complete::Shell) -> Result<String, ExitError> {
    let mut generated = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "porchlight", &mut generated);
    String::from_utf8(generated).map_err(|error| failed(&error))
}

fn failed(error: &dyn std::fmt::Display) -> ExitError {
    ExitError::new(ExitCode::Internal, error.to_string())
}

fn needs_person(what: &str) -> Result<(), ExitError> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }

    Err(ExitError::new(ExitCode::NeedsHuman, format!("{what} needs a person at this computer"))
        .next("Run it yourself in a terminal. An agent or script can't do this."))
}

fn say(json: bool, message: &str) {
    if json {
        out!("{}", json!({ "ok": true, "message": message }));
    } else {
        out!("✓ {message}");
    }
}

fn say_all(json: bool, title: &str, lines: &[String]) {
    if json {
        out!("{}", json!({ "ok": true, "message": title, "details": lines }));
    } else {
        out!("✓ {title}");

        for line in lines {
            out!("  {line}");
        }
    }
}

fn store() -> Result<Store, ExitError> {
    Store::open_default().map_err(|error| failed(&error))
}

async fn load() -> Result<(Config, Store, Snapshot), ExitError> {
    let config = Config::default_location();
    let store = store()?;
    let snap = snapshot::load(&config, &store).await;

    Ok((config, store, snap))
}

async fn saved(result: Result<(), ConfigError>) -> Result<(), ExitError> {
    result.map_err(|error| failed(&error))?;
    reload_daemon().await;
    Ok(())
}

fn tool_names(snap: &Snapshot) -> Vec<String> {
    snap.apps
        .iter()
        .flat_map(|app| {
            app.tool_names
                .iter()
                .map(move |tool| if *tool == app.name { tool.clone() } else { format!("{}/{tool}", app.name) })
        })
        .collect()
}

fn resolve(target: &str, snap: &Snapshot) -> Result<(String, Option<String>), ExitError> {
    if let Some((app, tool)) = target.split_once('/') {
        return Ok((app.to_owned(), Some(tool.to_owned())));
    }
    if snap.app_names().iter().any(|name| name == target) {
        return Ok((target.to_owned(), None));
    }
    let owners: Vec<&str> = snap
        .apps
        .iter()
        .filter(|app| app.tool_names.iter().any(|tool| tool == target))
        .map(|app| app.name.as_str())
        .collect();
    match owners.as_slice() {
        [] => Err(no_tool(target, snap)),
        [app] => Ok(((*app).to_owned(), Some(target.to_owned()))),
        several => Err(ExitError::new(
            ExitCode::NoServer,
            format!("{target} is a tool in {}. Say which one", several.join(" and ")),
        )
        .next(format!("For example: {}/{target}", several.first().copied().unwrap_or_default()))),
    }
}

fn no_tool(name: &str, snap: &Snapshot) -> ExitError {
    let names = tool_names(snap);
    let valid = if names.is_empty() { "none".to_owned() } else { names.join(", ") };
    ExitError::new(ExitCode::NoServer, format!("There's no tool called {name}. Tools: {valid}"))
        .next("See them with porchlight tools list.")
}

fn no_link(name: &str, snap: &Snapshot) -> ExitError {
    let valid = snap.links.iter().map(|link| link.name.as_str()).collect::<Vec<_>>().join(", ");
    ExitError::new(ExitCode::NoServer, format!("There's no link called {name}. Links: {valid}"))
        .next("See them with porchlight links list.")
}

fn state_word(state: Option<ServerState>) -> &'static str {
    match state {
        Some(ServerState::Live) => "live",
        Some(ServerState::Starting) => "starting",
        Some(ServerState::Waiting) => "waiting",
        Some(ServerState::Refused) => "refused",
        None => "not running",
    }
}

fn verdict_word(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "ok",
        Verdict::Refused => "refused",
        Verdict::Error => "error",
        Verdict::Change => "change",
        Verdict::Note => "",
    }
}

pub async fn run(command: Command, json: bool) -> Result<(), ExitError> {
    match command {
        Command::Serve => crate::run::serve().await,
        Command::Status => status(json).await,
        Command::Logs(args) => logs(args, json).await,
        Command::Start => start(json).await,
        Command::Stop => {
            crate::run::stop().await?;
            say(json, "porchlight stopped. The login service was removed.");
            Ok(())
        }
        Command::Tools { action } => match action {
            ToolsAction::List { link } => tools_list(link.as_deref(), json).await,
            ToolsAction::Add { name, read_only, description, link, env, command } => {
                add_command(name, command, read_only, description, link, env, json).await
            }
            ToolsAction::Edit { target, command, description, read_only } => {
                edit_tool(&target, command, description, read_only, json).await
            }
            ToolsAction::Remove { target, force } => remove_target(&target, force, json).await,
            ToolsAction::Enable { target } => switch(&target, true, json).await,
            ToolsAction::Disable { target } => switch(&target, false, json).await,
            ToolsAction::Try { target, values } => try_tool(&target, &values, json).await,
        },
        Command::Servers { action } => match action {
            ServersAction::List => servers_list(json).await,
            ServersAction::Add { name, url, header, env, command } => {
                servers_add(&name, url.as_deref(), &header, &env, &command, json).await
            }
            ServersAction::Import { names, all } => servers_import(&names, all, json).await,
            ServersAction::Remove { name, force } => remove_target(&name, force, json).await,
            ServersAction::Enable { name } => switch(&name, true, json).await,
            ServersAction::Disable { name } => switch(&name, false, json).await,
        },
        Command::Links { action } => match action {
            LinksAction::List => links(json).await,
            LinksAction::Create { name, tools } => link_add(&name, &tools, json).await,
            LinksAction::Edit { name, add, remove } => link_edit(&name, &add, &remove, json).await,
            LinksAction::Delete { name, force } => link_delete(&name, force, json).await,
        },
        Command::Approve { request } => client_approve(request, json).await,
        Command::Deny { request } => client_deny(request, json).await,
        Command::Clients { action } => match action {
            ClientsAction::List => clients(json).await,
            ClientsAction::Revoke { client, link, all, force } => {
                clients_revoke(client.as_deref(), link.as_deref(), all, force, json).await
            }
        },
        Command::Tokens { action } => match action {
            TokensAction::Create { link, name, expires } => token_create(&link, name.as_deref(), &expires, json).await,
            TokensAction::List => tokens_list(json).await,
            TokensAction::Revoke { target, force } => token_revoke(&target, force, json).await,
        },
        Command::Tunnel { action } => match action {
            None => tunnel(json).await,
            Some(TunnelAction::Use { tunnel }) => use_tunnel(&tunnel, json).await,
        },
        Command::Completions { shell } => {
            let generated = completions(shell)?;
            if json {
                out!("{}", json!({"ok": true, "completions": generated}));
            } else {
                out!("{generated}");
            }
            Ok(())
        }
    }
}

fn checked_name(name: &str, snap: &Snapshot) -> Result<(), ExitError> {
    match actions::name_problem(name, &snap.settings) {
        Some(problem) => Err(ExitError::new(ExitCode::NoServer, problem)),
        None => Ok(()),
    }
}

fn pairs(values: &[String]) -> Result<Vec<(String, String)>, ExitError> {
    values
        .iter()
        .map(|pair| {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(ExitError::new(ExitCode::NoServer, format!("{pair} must be KEY=VALUE")));
            };
            if key.is_empty() {
                return Err(ExitError::new(ExitCode::NoServer, format!("{pair} must be KEY=VALUE")));
            }
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn confirm(question: &str, force: bool) -> Result<(), ExitError> {
    if force {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(ExitError::new(ExitCode::NoServer, "This command removes access or configuration")
            .next("Run it again with --force."));
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|error| failed(&error))?;
    if answer.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err(ExitError::new(ExitCode::NoServer, "Nothing was removed"))
    }
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| if character.is_control() && !matches!(character, '\n' | '\t') { '�' } else { character })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn every_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn try_output_replaces_control_characters_except_tabs_and_newlines() {
        assert_eq!(super::terminal_text("ok\u{1b}[31m\tline\nnext\u{7f}"), "ok�[31m\tline\nnext�");
    }
}
