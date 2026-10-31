use super::{confirm, failed, load, needs_person, no_link, say};
use crate::actions;
use crate::auth::Auth;
use crate::crypto::parse_duration;
use crate::exit::{ExitCode, ExitError};
use crate::snapshot::{self, Snapshot};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

pub(super) async fn client_approve(request: Option<String>, json: bool) -> Result<(), ExitError> {
    needs_person("Approving a client")?;
    let (_, _, snap) = load().await?;
    let pending = pick_request(&snap, request)?;
    if json {
        out!(
            "{}",
            json!({
                "ok": true, "request": pending.id, "client": pending.client, "verified": pending.verified,
                "link": pending.link, "code": pending.code
            })
        );
    } else {
        out!("Code for {} on {}: {}", pending.label(), pending.link, pending.code);
        out!("Type it on the device that's connecting. It expires in {}.", pending.left);
    }
    Ok(())
}

pub(super) async fn client_deny(request: Option<String>, json: bool) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;
    let pending = pick_request(&snap, request)?;
    actions::deny(&store, &pending.id).map_err(|error| failed(&error))?;
    say(json, &format!("Denied {} on {}", pending.client, pending.link));
    Ok(())
}

pub(super) async fn clients_revoke(
    client: Option<&str>,
    link: Option<&str>,
    all: bool,
    force: bool,
    json: bool,
) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;
    if all {
        let count = snap.clients.len();
        confirm(&format!("Revoke every client and token ({count})?"), force)?;
        Auth::new(store).revoke_all().map_err(|error| failed(&error))?;
        say(json, &format!("Revoked every client and token ({count})"));
        return Ok(());
    }
    let matched: Vec<&snapshot::ClientInfo> = snap
        .clients
        .iter()
        .filter(|info| !info.token)
        .filter(|info| {
            info.id == client.unwrap_or_default() || info.name.eq_ignore_ascii_case(client.unwrap_or_default())
        })
        .filter(|info| link.is_none_or(|link| info.link == link))
        .collect();
    if matched.is_empty() {
        let valid = snap
            .clients
            .iter()
            .filter(|info| !info.token)
            .map(|info| format!("{} ({})", info.name, info.id))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExitError::new(
            ExitCode::NoServer,
            format!("No approved client called {}. Clients: {valid}", client.unwrap_or_default()),
        )
        .next("See them with porchlight clients list."));
    }
    let targets = matched.iter().map(|info| format!("{} on {}", info.name, info.link)).collect::<Vec<_>>().join(", ");
    confirm(&format!("Revoke {targets}?"), force)?;
    let auth = Auth::new(store);
    for info in &matched {
        auth.revoke_grant(&info.id).map_err(|error| failed(&error))?;
    }
    say(json, &format!("Revoked {} client(s)", matched.len()));
    Ok(())
}

pub(super) async fn token_create(link: &str, name: Option<&str>, expires: &str, json: bool) -> Result<(), ExitError> {
    needs_person("Creating a token")?;
    let (_, store, snap) = load().await?;
    if !snap.links.iter().any(|info| info.name == link) {
        return Err(no_link(link, &snap));
    }
    let ttl = parse_duration(expires)
        .ok_or_else(|| ExitError::new(ExitCode::NoServer, format!("\"{expires}\" isn't a duration")))?;
    let name = name.unwrap_or("automation");
    let (id, token) = Auth::new(store).create_static_token(link, name, ttl).map_err(|error| failed(&error))?;
    if json {
        out!("{}", json!({"ok": true, "id": id, "name": name, "link": link, "token": token}));
    } else {
        out!("Token {name} for {link} ({id}): {token}");
    }
    Ok(())
}

pub(super) async fn tokens_list(json: bool) -> Result<(), ExitError> {
    let (_, store, _) = load().await?;
    let tokens = store.tokens_list_static().map_err(|error| failed(&error))?;
    if json {
        out!(
            "{}",
            json!({"ok": true, "tokens": tokens.iter().map(|token| json!({
            "id": token.id, "name": token.name, "link": token.server, "expires": token.expires_at
        })).collect::<Vec<_>>()})
        );
    } else if tokens.is_empty() {
        out!("No tokens yet. Create one with porchlight tokens create --link <link>.");
    } else {
        for token in tokens {
            out!("{} {} {}", token.name, token.server, token.id);
        }
    }
    Ok(())
}

pub(super) async fn token_revoke(target: &str, force: bool, json: bool) -> Result<(), ExitError> {
    let (_, store, _) = load().await?;
    let tokens = store.tokens_list_static().map_err(|error| failed(&error))?;
    let matching: Vec<_> = tokens.iter().filter(|token| token.id == target || token.name == target).collect();
    if matching.is_empty() {
        let valid = tokens.iter().map(|token| format!("{} ({})", token.name, token.id)).collect::<Vec<_>>().join(", ");
        return Err(ExitError::new(ExitCode::NoServer, format!("No token called {target}. Tokens: {valid}"))
            .next("See them with porchlight tokens list."));
    }
    if matching.len() > 1 {
        let ids = matching.iter().map(|token| token.id.as_str()).collect::<Vec<_>>().join(", ");
        return Err(ExitError::new(ExitCode::NoServer, format!("Token name {target} is ambiguous. IDs: {ids}"))
            .next("Choose one with porchlight tokens revoke ID."));
    }
    let token = matching.first().copied().ok_or_else(|| failed(&"No token matched"))?;
    confirm(&format!("Revoke token {} for {}?", token.name, token.server), force)?;
    Auth::new(store).revoke_grant(&token.id).map_err(|error| failed(&error))?;
    say(json, &format!("Revoked token {}", token.name));
    Ok(())
}

pub(super) async fn clients(json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;
    let approved: Vec<&snapshot::ClientInfo> = snap.clients.iter().filter(|client| !client.token).collect();
    let pending: Vec<Value> = snap
        .pending
        .iter()
        .map(|request| {
            json!({
                "request": request.id, "client": request.client, "verified": request.verified,
                "link": request.link, "asked": request.asked, "code": request.code
            })
        })
        .collect();
    let clients: Vec<Value> = approved
        .iter()
        .map(|client| {
            json!({
                "id": client.id, "name": client.name, "link": client.link, "last_used": client.last_used
            })
        })
        .collect();
    if json {
        out!("{}", json!({ "ok": true, "waiting": pending, "clients": clients }));
        return Ok(());
    }
    for request in &snap.pending {
        out!(
            "! {} wants {} · {} · {} · code {}",
            request.label(),
            request.link,
            request.asked,
            request.id,
            request.code
        );
    }
    if !snap.pending.is_empty() {
        out!("  approve with porchlight approve");
    }
    if approved.is_empty() {
        out!("No approved clients yet.");
    }
    for client in approved {
        out!("● {:<30} {:<12} {:<14} {}", client.name, client.link, client.last_used, client.id);
    }
    Ok(())
}

pub(super) fn pick_request(snap: &Snapshot, request: Option<String>) -> Result<snapshot::PendingInfo, ExitError> {
    if let Some(id) = request {
        return snap.pending.iter().find(|pending| pending.id == id).cloned().ok_or_else(|| {
            ExitError::new(ExitCode::NoServer, format!("No waiting request {id}"))
                .next("See them with porchlight clients list.")
        });
    }
    match snap.pending.as_slice() {
        [] => Err(ExitError::new(ExitCode::NoServer, "Nothing is waiting")),
        [only] => Ok(only.clone()),
        pending if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            for (index, info) in pending.iter().enumerate() {
                eprintln!("{}: {} on {} ({})", index + 1, info.label(), info.link, info.id);
            }
            eprint!("Choose request number: ");
            let _ = std::io::stderr().flush();
            let mut value = String::new();
            std::io::stdin().read_line(&mut value).map_err(|error| failed(&error))?;
            let selection = value.trim().parse::<usize>().ok().and_then(|index| index.checked_sub(1));
            selection
                .and_then(|index| pending.get(index).cloned())
                .ok_or_else(|| ExitError::new(ExitCode::NoServer, "That request number isn't valid"))
        }
        pending => {
            let choices =
                pending.iter().map(|info| format!("{} ({})", info.client, info.id)).collect::<Vec<_>>().join(", ");
            Err(ExitError::new(ExitCode::NoServer, format!("{} requests are waiting: {choices}", pending.len()))
                .next("Pick one with porchlight approve REQUEST."))
        }
    }
}
