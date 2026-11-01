use super::{checked_name, failed, load, pairs, say_all, state_word};
use crate::actions;
use crate::core::is_http_url;
use crate::exit::{ExitCode, ExitError};
use serde_json::{Value, json};

pub(super) async fn servers_add(
    name: &str,
    url: Option<&str>,
    headers: &[String],
    env: &[String],
    command: &[String],
    json: bool,
) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    checked_name(name, &snap)?;
    let mut server = serde_json::Map::new();
    if let Some(url) = url {
        if !is_http_url(url) {
            return Err(ExitError::new(ExitCode::NoServer, format!("{url} isn't an http:// or https:// address")));
        }
        server.insert("url".to_owned(), json!(url));
        let headers: serde_json::Map<String, Value> =
            pairs(headers)?.into_iter().map(|(key, value)| (key, json!(value))).collect();
        if !headers.is_empty() {
            server.insert("headers".to_owned(), Value::Object(headers));
        }
    } else {
        if command.is_empty() {
            return Err(ExitError::new(ExitCode::NoServer, "Give the server's --url, or the command that starts it")
                .next(format!("porchlight servers add {name} --url http://127.0.0.1:8080/mcp, or porchlight servers add {name} -- <command>")));
        }
        if let Some(problem) = actions::program_problem(command.first().map(String::as_str).unwrap_or_default()) {
            return Err(ExitError::new(ExitCode::NoServer, problem));
        }
        server.insert("command".to_owned(), crate::spec::run_value(command));
        let environment: serde_json::Map<String, Value> =
            pairs(env)?.into_iter().map(|(key, value)| (key, json!(value))).collect();
        if !environment.is_empty() {
            server.insert("environment".to_owned(), Value::Object(environment));
        }
    }
    let shared = actions::share_server(&config, name, &Value::Object(server)).await.map_err(|error| failed(&error))?;
    say_all(json, &format!("Added {name}"), &crate::explain::shared(&shared));
    Ok(())
}

pub(super) fn found_reason(server: &crate::discovery::DiscoveredServer) -> &'static str {
    if !server.local {
        "not on this computer"
    } else if server.exposable() {
        ""
    } else {
        "not running"
    }
}

pub(super) async fn servers_import(names: &[String], all: bool, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let found: Vec<(String, crate::discovery::DiscoveredServer)> = crate::discovery::discover(&snap.settings)
        .await
        .into_iter()
        .filter(|server| server.source != "porchlight.json")
        .map(|server| (actions::import_name(&server, &snap.settings), server))
        .collect();
    if names.is_empty() && !all {
        if json {
            let listed: Vec<Value> = found
                .iter()
                .map(|(name, server)| json!({ "name": name, "found": server, "ready": server.exposable() }))
                .collect();
            out!("{}", json!({ "ok": true, "found": listed }));
            return Ok(());
        }
        if found.is_empty() {
            out!("Nothing new in Claude, Cursor or VS Code settings.");
            out!("  Add one yourself with porchlight servers add <name> -- <command>");
            return Ok(());
        }
        let width = found.iter().map(|(name, _)| name.chars().count()).max().unwrap_or_default();
        let from = found.iter().map(|(_, server)| server.source.chars().count()).max().unwrap_or_default();
        for (name, server) in &found {
            let what =
                server.url.clone().or_else(|| server.command.as_ref().map(|words| words.join(" "))).unwrap_or_default();
            let reason = found_reason(server);
            let reason = if reason.is_empty() { String::new() } else { format!("  ({reason})") };
            out!("{name:<width$}  {:<from$}  {what}{reason}", server.source);
        }
        out!("Share one with porchlight servers import <name>, or every one with --all.");
        return Ok(());
    }
    let chosen: Vec<&(String, crate::discovery::DiscoveredServer)> = if all {
        found.iter().filter(|(_, server)| server.exposable()).collect()
    } else {
        names
            .iter()
            .map(|wanted| {
                let listed = found.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
                let item = found.iter().find(|(name, _)| name == wanted).ok_or_else(|| {
                    ExitError::new(
                        ExitCode::NoServer,
                        format!(
                            "porchlight didn't find {wanted}. Found: {}",
                            if listed.is_empty() { "none" } else { &listed }
                        ),
                    )
                    .next("See them with porchlight servers import.")
                })?;
                let reason = found_reason(&item.1);
                if reason.is_empty() {
                    Ok(item)
                } else {
                    Err(ExitError::new(ExitCode::NoServer, format!("{wanted} is {reason}, so it can't be shared")))
                }
            })
            .collect::<Result<_, _>>()?
    };
    if chosen.is_empty() {
        return Err(ExitError::new(ExitCode::NoServer, "Nothing found that runs on this computer")
            .next("See what was found with porchlight servers import."));
    }
    let mut lines = Vec::new();
    for (name, server) in &chosen {
        let Some(entry) = server.entry() else { continue };
        let value = serde_json::to_value(&entry).map_err(|error| failed(&error))?;
        lines.push(format!("{name} from {}", server.source));
        let shared = actions::share_server(&config, name, &value).await.map_err(|error| failed(&error))?;
        lines.extend(crate::explain::shared(&shared));
    }
    let shared = chosen.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
    say_all(json, &format!("Shared {shared}"), &lines);
    Ok(())
}

pub(super) async fn servers_list(json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;
    let servers: Vec<(&str, bool, &[String], &str, &str)> = snap
        .apps
        .iter()
        .filter(|app| !app.command)
        .map(|app| (app.name.as_str(), app.on, app.links.as_slice(), state_word(app.state), app.detail.as_str()))
        .collect();
    if json {
        out!(
            "{}",
            json!({"ok": true, "servers": servers.iter().map(|(name, on, links, state, detail)| json!({
            "name": name, "on": on, "links": links, "state": state, "detail": detail
        })).collect::<Vec<_>>()})
        );
    } else {
        let width = servers.iter().map(|(name, ..)| name.chars().count()).max().unwrap_or_default();
        for (name, on, _, state, detail) in &servers {
            let about =
                if detail.is_empty() || state == &"not running" { (*state).to_owned() } else { (*detail).to_owned() };
            out!("{:<4}{name:<width$}  {about}", if *on { "on" } else { "off" });
        }
        if servers.is_empty() {
            out!("No MCP servers yet. Find the ones your clients use with porchlight servers import");
        }
    }
    Ok(())
}
