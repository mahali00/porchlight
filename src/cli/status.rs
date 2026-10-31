use super::{LogsArgs, failed, load, no_link, no_tool, say, say_all, verdict_word};
use crate::actions;
use crate::control::set_daemon_tunnel;
use crate::core::is_https_url;
use crate::exit::{ExitCode, ExitError};
use crate::run::tunnel_trouble;
use crate::snapshot::{self, LogRow, Verdict};
use crate::tunnels::Provider;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::time::Duration;

pub(super) async fn status(json: bool) -> Result<(), ExitError> {
    crate::run::status(json).await?;
    if !json {
        let (_, _, snap) = load().await?;
        let waiting = snap.pending.len();
        out!("  {waiting} waiting · {} approved", snap.clients.len());

        if snap.links.is_empty() {
            out!("  Next: porchlight tools add <name> -- <command> [args...]");
        } else if snap.live.is_none() {
            out!("  Next: porchlight start");
        } else if waiting > 0 {
            out!("  Next: porchlight approve");
        }
    }
    Ok(())
}

pub(super) fn log_json(row: &LogRow) -> Value {
    json!({
        "day": row.day,
        "time": row.time,
        "client": row.client,
        "app": row.link,
        "what": row.what,
        "result": verdict_word(row.result),
        "detail": row.detail,
        "input": row.input,
    })
}

pub(super) fn log_line(row: &LogRow) -> String {
    let mark = match row.result {
        Verdict::Ok => "✓",
        Verdict::Refused | Verdict::Error => "✗",
        Verdict::Change => "•",
        Verdict::Note => "·",
    };

    format!("{:<9} {}  {mark} {:<18} {:<12} {:<26} {}", row.day, row.time, row.client, row.link, row.what, row.detail)
        .trim_end()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

pub(super) async fn logs(args: LogsArgs, json: bool) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;
    if let Some(tool) = &args.tool
        && !snap.apps.iter().any(|app| app.tool_names.iter().any(|name| name == tool))
    {
        return Err(no_tool(tool, &snap));
    }
    let members: Option<Vec<String>> = match &args.link {
        Some(link) => {
            let plan = snap.plans.iter().find(|plan| plan.name() == link).ok_or_else(|| no_link(link, &snap))?;
            Some(plan.members().into_iter().chain([link.clone()]).collect())
        }
        None => None,
    };
    let client = args.client.as_ref().map(|client| client.to_lowercase());
    let wanted = |row: &LogRow| {
        args.tool.as_ref().is_none_or(|tool| row.what.contains(tool))
            && members.as_ref().is_none_or(|members| members.contains(&row.link))
            && client.as_ref().is_none_or(|client| row.client.to_lowercase().contains(client.as_str()))
    };
    let print = |row: &LogRow| out!("{}", log_line(row));
    let now = crate::core::now_ms();
    let entries = store.audit_list(5000).map_err(|error| failed(&error))?;
    let names = snapshot::ClientNames::new(&store);
    let mut last = entries.first().map_or(0, |entry| entry.id);
    let mut recent: Vec<LogRow> = entries
        .iter()
        .map(|entry| snapshot::log_row(entry, &names, now))
        .filter(|row| wanted(row))
        .take(args.lines)
        .collect();
    recent.reverse();
    if json {
        out!("{}", json!({"ok": true, "logs": recent.iter().map(log_json).collect::<Vec<_>>()}));
        return Ok(());
    }
    recent.iter().for_each(print);
    if args.once || !(args.follow || std::io::stdout().is_terminal()) {
        return Ok(());
    }

    if !json && recent.is_empty() {
        out!("· waiting for calls, ctrl-c stops");
    }

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let fresh = store.audit_after(last).map_err(|error| failed(&error))?;
        let now = crate::core::now_ms();

        for entry in &fresh {
            last = entry.id;
            let row = snapshot::log_row(entry, &names, now);

            if wanted(&row) {
                print(&row);
            }
        }
    }
}

pub(super) async fn start(json: bool) -> Result<(), ExitError> {
    let quiet = |_: &str| {};
    let loud = |step: &str| eprintln!("· {step}");
    let (state, installed) = crate::run::start(if json { &quiet } else { &loud }).await.map_err(|mut error| {
        if let Some(trouble) = tunnel_trouble() {
            error.next_action = Some(trouble);
        }
        error
    })?;
    let mut lines = vec![if installed {
        "A launchd or systemd login service is installed and starts porchlight at login.".to_owned()
    } else {
        "Sharing is running for this login; no login service was installed.".to_owned()
    }];
    lines.extend(state.servers.iter().map(|server| format!("{:<12} {}", server.name, server.url)));
    say_all(json, "porchlight is sharing", &lines);

    Ok(())
}

pub(super) async fn tunnel(json: bool) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;
    let address = snap.live.as_ref().map(|live| live.public_url.clone());
    let port = store.ports().map_err(|error| failed(&error))?.gateway;

    if json {
        out!("{}", json!({ "ok": true, "tunnel": snap.tunnel, "address": address, "port": port }));
        return Ok(());
    }

    out!("tunnel   {}", snap.tunnel);
    out!("address  {}", address.unwrap_or_else(|| "not running · start with porchlight start".to_owned()));
    out!("port     127.0.0.1:{port} · point your own tunnel here");
    Ok(())
}

pub(super) async fn use_tunnel(tunnel: &str, json: bool) -> Result<(), ExitError> {
    let tunnel = tunnel.trim_end_matches('/');

    if Provider::named(tunnel).is_none() && !is_https_url(tunnel) {
        return Err(ExitError::new(ExitCode::TunnelFailed, format!("{tunnel} isn't a tunnel porchlight knows"))
            .next("Use opentunnel, or your own tunnel's https:// address."));
    }

    let (config, _, snap) = load().await?;
    actions::set_tunnel(&config, tunnel).map_err(|error| failed(&error))?;

    if snap.live.is_none() {
        say(json, &format!("Saved {tunnel}. porchlight uses it the next time it starts."));
        return Ok(());
    }

    let state = set_daemon_tunnel(tunnel).await.map_err(|error| {
        ExitError::new(ExitCode::TunnelFailed, format!("The tunnel was saved but didn't open: {error}"))
            .next(tunnel_trouble().unwrap_or_else(|| "Check again with porchlight tunnel.".to_owned()))
    })?;
    say(json, &format!("Clients reach porchlight at {}", state.public_url));
    Ok(())
}
