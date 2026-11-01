use super::{confirm, failed, load, no_link, no_tool, saved, say, state_word};
use crate::actions;
use crate::core::is_app_name;
use crate::exit::{ExitCode, ExitError};
use crate::links::ServerState;
use serde_json::{Value, json};

pub(super) async fn link_edit(link: &str, add: &[String], remove: &[String], json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let members = snap.settings.links.get(link).ok_or_else(|| no_link(link, &snap))?;
    if members.is_all() {
        return Err(ExitError::new(ExitCode::NoServer, format!("{link} includes every command tool")));
    }
    let original = members.apps().to_vec();
    let mut current = original.clone();
    for name in remove {
        if !current.contains(name) {
            return Err(no_tool(name, &snap));
        }
        current.retain(|item| item != name);
    }
    for name in add {
        if !snap.settings.commands.contains_key(name) {
            return Err(no_tool(name, &snap));
        }
        if !current.contains(name) {
            current.push(name.clone());
        }
    }
    if current == original {
        return Err(ExitError::new(ExitCode::NoServer, "No link tools changed"));
    }
    saved(actions::set_members(&config, link, &current)).await?;
    say(json, &format!("Updated {link}"));
    Ok(())
}

pub(super) async fn link_delete(link: &str, force: bool, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    if !snap.settings.links.contains_key(link) {
        return Err(no_link(link, &snap));
    }
    confirm(&format!("Delete link {link} and revoke its clients?"), force)?;
    saved(actions::remove_link(&config, link)).await?;
    say(json, &format!("Deleted {link}"));
    Ok(())
}

pub(super) async fn links(json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;

    if json {
        let links: Vec<Value> = snap
            .links
            .iter()
            .map(|link| {
                json!({
                    "name": link.name,
                    "address": link.address,
                    "state": state_word(link.state),
                    "apps": link.groups.iter().map(|group| &group.name).collect::<Vec<_>>(),
                    "tools_on": link.on,
                    "tools": link.total,
                    "clients": link.clients,
                })
            })
            .collect();
        out!("{}", json!({ "ok": true, "links": links }));
        return Ok(());
    }

    if snap.links.is_empty() {
        out!("No links yet. Add a tool with porchlight tools add <name> -- <command>.");
    }

    for link in &snap.links {
        let apps: Vec<&str> = link.groups.iter().map(|group| group.name.as_str()).collect();
        out!(
            "{} {:<12} {}",
            if link.state == Some(ServerState::Live) { "●" } else { "○" },
            link.name,
            link.address.as_deref().unwrap_or(state_word(link.state))
        );
        out!(
            "  {} · {} of {} tools on · {}",
            apps.join(", "),
            link.on,
            link.total,
            match link.clients.len() {
                0 => "no clients".to_owned(),
                1 => "1 client".to_owned(),
                count => format!("{count} clients"),
            }
        );
    }

    Ok(())
}

pub(super) async fn link_add(link: &str, apps: &[String], json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;

    if !is_app_name(link) || snap.settings.mcp.contains_key(link) {
        return Err(ExitError::new(ExitCode::NoServer, format!("\"{link}\" can't be a link name"))
            .next("Use lowercase letters, numbers and -, and not the name of an MCP server."));
    }

    if let Some(app) = apps.iter().find(|app| !snap.settings.commands.contains_key(*app)) {
        return Err(if snap.settings.mcp.contains_key(app) {
            ExitError::new(ExitCode::NoServer, format!("{app} is an MCP server, so it has its own link"))
                .next("See it with porchlight servers list.")
        } else {
            no_tool(app, &snap)
        });
    }

    match snap.settings.links.get(link) {
        Some(members) if members.is_all() => {
            return Err(ExitError::new(ExitCode::NoServer, format!("{link} already has every command-line tool")));
        }
        Some(members) => {
            let mut kept = members.apps().to_vec();
            kept.extend(apps.iter().filter(|app| !members.apps().contains(app)).cloned());
            saved(actions::set_members(&config, link, &kept)).await?;
        }
        None => saved(actions::new_link(&config, link, apps)).await?,
    }

    let members = config
        .load()
        .map_err(|error| failed(&error))?
        .settings
        .links
        .get(link)
        .map(|members| members.apps().to_vec())
        .unwrap_or_default();
    say(
        json,
        &if members.is_empty() { format!("Created {link}") } else { format!("{link} has {}", members.join(", ")) },
    );
    Ok(())
}
