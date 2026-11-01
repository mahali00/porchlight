use crate::actions::Shared;
use crate::spec::CommandSpec;

pub fn shared(shared: &Shared) -> Vec<String> {
    let app = &shared.app;
    let mut lines: Vec<String> = shared
        .created
        .iter()
        .map(|link| format!("There was no link called {link}, so porchlight created it."))
        .collect();
    let placed: Vec<&str> = shared.placements.iter().map(|placement| placement.link.as_str()).collect();

    if let Some(problem) = &shared.problem {
        lines.push(format!("Couldn't share {app}: {problem}"));
    } else if placed.is_empty() {
        lines.push(if shared.running {
            format!("Saved {app}. Add it to a link with porchlight links create <name> --tool {app}.")
        } else {
            format!("Saved {app}. It isn't on a link yet.")
        });
    } else if !shared.running {
        lines.push(if placed == [app.as_str()] {
            format!("Saved {app}. It goes live when porchlight starts.")
        } else {
            format!("Saved {app} on {}. It goes live when porchlight starts.", placed.join(", "))
        });
    } else {
        lines.extend(shared.placements.iter().filter_map(|placement| {
            placement.address.as_ref().map(|address| format!("Add {address} in your MCP client, then approve it here."))
        }));
    }

    if !shared.turned_off.is_empty() {
        lines.push(format!("▲ {} can run code, so they're off until enabled.", shared.turned_off.join(", ")));
    }

    if shared.reapproval {
        lines.push("Clients on this link must approve again after its tools change.".to_owned());
    }

    lines
}

pub fn caution(spec: &CommandSpec) -> Vec<String> {
    let risk = spec.risk();
    let chosen = risk.open.iter().map(|name| format!("{{{name}}}")).collect::<Vec<_>>().join(", ");
    let first = risk.open.first().cloned().unwrap_or_default();

    vec![
        format!("{} {}, and clients choose {chosen}.", spec.program_name(), risk.effect.unwrap_or("can change things")),
        "A client you approve on its link decides what it acts on.".to_owned(),
        "Clients are told it can change things, so most ask before each call.".to_owned(),
        format!("Safer: give {{{first}}} a few choices in porchlight.json, or keep the risky part fixed,"),
        "like  ./cleanup.sh ~/old/{name}  instead of  ./cleanup.sh {path}.".to_owned(),
    ]
}
