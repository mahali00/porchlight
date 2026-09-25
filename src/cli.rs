use crate::actions::{self, CommandDraft};
use crate::apps::{command_words, host_environment};
use crate::auth::Auth;
use crate::config::{Config, ConfigError};
use crate::control::{reload_daemon, set_daemon_tunnel};
use crate::core::{is_app_name, is_http_url, is_https_url};
use crate::crypto::parse_duration;
use crate::exit::{ExitCode, ExitError};
use crate::links::ServerState;
use crate::run::{ensure_tunnel, tunnel_trouble};
use crate::snapshot::{self, LogRow, Snapshot, Verdict};
use crate::store::Store;
use crate::system;
use crate::tunnels::Provider;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Subcommand)]
pub enum Command {
    #[command(hide = true, about = "Run the daemon in the foreground, as the background service does")]
    Serve,
    #[command(about = "What's shared, who's connected, what's waiting")]
    Status,
    #[command(about = "Recent calls, then keeps following in a terminal")]
    Logs(LogsArgs),
    #[command(about = "Start sharing in the background")]
    Start,
    #[command(about = "Stop sharing and remove the background service")]
    Stop,
    #[command(about = "List apps, or add and remove them")]
    Apps {
        #[command(subcommand)]
        action: Option<AppsAction>,
    },
    #[command(about = "Turn an app, or one of its tools, on")]
    On { target: String },
    #[command(about = "Turn an app, or one of its tools, off")]
    Off { target: String },
    #[command(about = "Run a tool once and print what it returns (read-only tools for MCP servers)")]
    Try {
        #[arg(help = "app or app/tool")]
        target: String,
        #[arg(help = "inputs as name=value")]
        inputs: Vec<String>,
    },
    #[command(about = "List links, or change which apps they share")]
    Links {
        #[command(subcommand)]
        action: Option<LinksAction>,
    },
    #[command(about = "Approved clients and waiting requests")]
    Clients {
        #[command(subcommand)]
        action: Option<ClientsAction>,
    },
    #[command(about = "How clients reach this computer")]
    Tunnel {
        #[command(subcommand)]
        action: Option<TunnelAction>,
    },
}

#[derive(Args)]
pub struct LogsArgs {
    #[arg(short, long, help = "only calls to this app")]
    app: Option<String>,
    #[arg(short, long, help = "only calls on this link")]
    link: Option<String>,
    #[arg(short, long, help = "only calls from this client")]
    client: Option<String>,
    #[arg(short = 'n', long, default_value_t = 20, help = "how many recent lines to show")]
    lines: usize,
    #[arg(short, long, help = "keep following even when the output isn't a terminal")]
    follow: bool,
    #[arg(long, conflicts_with = "follow", help = "print recent lines and stop")]
    once: bool,
}

#[derive(Subcommand)]
pub enum AppsAction {
    #[command(about = "Add a command-line tool or an MCP server")]
    Add {
        #[arg(help = "name for the app, picked from the command or address when left out")]
        name: Option<String>,
        #[arg(short, long, group = "kind", help = "a command-line tool, like \"battery --json\"")]
        run: Option<String>,
        #[arg(short, long, group = "kind", help = "an MCP server at an address")]
        url: Option<String>,
        #[arg(short, long, group = "kind", help = "an MCP server started by a command")]
        server: Option<String>,
        #[arg(short, long, requires = "run", help = "what the tool does")]
        description: Option<String>,
        #[arg(long, requires = "run", help = "the tool only reads, it never changes anything")]
        reads: bool,
        #[arg(short, long, requires = "run", help = "put it on this link, repeat for more")]
        link: Vec<String>,
    },
    #[command(about = "Change a command-line tool's command, description or whether it only reads")]
    Edit {
        #[arg(help = "app or app/tool")]
        target: String,
        #[arg(short, long, help = "the new command")]
        run: Option<String>,
        #[arg(short, long, help = "what the tool does")]
        description: Option<String>,
        #[arg(long, conflicts_with = "changes", help = "it only reads")]
        reads: bool,
        #[arg(long, help = "it changes things")]
        changes: bool,
    },
    #[command(about = "Remove an app from porchlight")]
    Remove { app: String },
}

#[derive(Subcommand)]
pub enum LinksAction {
    #[command(about = "Create a link, or add apps to one")]
    Add {
        link: String,
        #[arg(short, long, help = "an app to put on the link, repeat for more")]
        app: Vec<String>,
    },
    #[command(about = "Remove apps from a link, or the whole link")]
    Remove {
        link: String,
        #[arg(short, long, help = "only take this app off, repeat for more")]
        app: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ClientsAction {
    #[command(about = "Approve a waiting request (a person at this computer only)")]
    Approve {
        #[arg(short, long, help = "which request, when more than one is waiting")]
        request: Option<String>,
        #[arg(long, help = "show a code for a phone or another computer instead of opening the browser")]
        code: bool,
    },
    #[command(about = "Deny a waiting request")]
    Deny {
        #[arg(short, long, help = "which request, when more than one is waiting")]
        request: Option<String>,
    },
    #[command(about = "Take away a client's access")]
    Revoke {
        #[arg(short, long, required_unless_present = "everyone", help = "the client's name or id")]
        client: Option<String>,
        #[arg(short, long, help = "only on this link")]
        link: Option<String>,
        #[arg(long, conflicts_with_all = ["client", "link"], help = "every client and token")]
        everyone: bool,
    },
    #[command(about = "Create a token for an automation (a person at this computer only)")]
    Token {
        #[arg(short, long)]
        link: String,
        #[arg(short, long, default_value = "automation")]
        name: String,
        #[arg(long, default_value = "30d", help = "how long it lasts, like 30d, 12h or 60m")]
        lasts: String,
    },
}

#[derive(Subcommand)]
pub enum TunnelAction {
    #[command(about = "Switch to opentunnel, or your own https:// address")]
    Use { tunnel: String },
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

fn no_app(name: &str) -> ExitError {
    ExitError::new(ExitCode::NoServer, format!("There's no app called {name}")).next("See them with porchlight apps.")
}

fn no_link(name: &str) -> ExitError {
    ExitError::new(ExitCode::NoServer, format!("There's no link called {name}")).next("See them with porchlight links.")
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
            say(json, "porchlight stopped. Approved clients can reconnect when you share again.");
            Ok(())
        }
        Command::Apps { action: None } => apps(json).await,
        Command::Apps { action: Some(AppsAction::Remove { app }) } => remove_app(&app, json).await,
        Command::Apps { action: Some(AppsAction::Edit { target, run, description, reads, changes }) } => {
            let read_only = if reads {
                Some(true)
            } else if changes {
                Some(false)
            } else {
                None
            };
            edit_tool(&target, run, description, read_only, json).await
        }
        Command::Apps { action: Some(AppsAction::Add { name, run, url, server, description, reads, link }) } => {
            let draft = AddDraft { name, description, reads, links: link };
            match (run, url, server) {
                (Some(run), _, _) => add_command(draft, &run, json).await,
                (_, Some(url), _) => add_url(draft.name, &url, json).await,
                (_, _, Some(server)) => add_server(draft.name, &server, json).await,
                _ => Err(ExitError::new(ExitCode::NoServer, "Say what to add")
                    .next("porchlight apps add --run \"battery --json\", --url http://…, or --server \"npx -y …\"")),
            }
        }
        Command::On { target } => switch(&target, true, json).await,
        Command::Off { target } => switch(&target, false, json).await,
        Command::Try { target, inputs } => try_tool(&target, &inputs, json).await,
        Command::Links { action: None } => links(json).await,
        Command::Links { action: Some(LinksAction::Add { link, app }) } => link_add(&link, &app, json).await,
        Command::Links { action: Some(LinksAction::Remove { link, app }) } => link_remove(&link, &app, json).await,
        Command::Clients { action: None } => clients(json).await,
        Command::Clients { action: Some(action) } => client_action(action, json).await,
        Command::Tunnel { action: None } => tunnel(json).await,
        Command::Tunnel { action: Some(TunnelAction::Use { tunnel }) } => use_tunnel(&tunnel, json).await,
    }
}

async fn status(json: bool) -> Result<(), ExitError> {
    crate::run::status(json).await?;

    if !json {
        let (_, _, snap) = load().await?;
        let waiting = snap.pending.len();

        if waiting > 0 {
            out!("  ! {waiting} waiting · porchlight clients");
        }

        out!("  {} approved", snap.clients.len());
    }

    Ok(())
}

fn log_json(row: &LogRow) -> Value {
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

fn log_line(row: &LogRow) -> String {
    let mark = match row.result {
        Verdict::Ok => "✓",
        Verdict::Refused | Verdict::Error => "✗",
        Verdict::Change => "•",
        Verdict::Note => "·",
    };

    format!("{:<9} {}  {mark} {:<18} {:<12} {:<26} {}", row.day, row.time, row.client, row.link, row.what, row.detail)
        .trim_end()
        .to_owned()
}

async fn logs(args: LogsArgs, json: bool) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;

    if let Some(app) = &args.app
        && !snap.app_names().contains(app)
    {
        return Err(no_app(app));
    }

    let members: Option<Vec<String>> = match &args.link {
        Some(link) => {
            let plan = snap.plans.iter().find(|plan| plan.name() == link).ok_or_else(|| no_link(link))?;
            Some(plan.members().into_iter().chain([link.clone()]).collect())
        }
        None => None,
    };
    let client = args.client.as_ref().map(|client| client.to_lowercase());
    let wanted = |row: &LogRow| {
        args.app.as_ref().is_none_or(|app| row.link == *app)
            && members.as_ref().is_none_or(|members| members.contains(&row.link))
            && client.as_ref().is_none_or(|client| row.client.to_lowercase().contains(client.as_str()))
    };
    let print = |row: &LogRow| {
        if json {
            out!("{}", log_json(row));
        } else {
            out!("{}", log_line(row));
        }
    };
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

async fn start(json: bool) -> Result<(), ExitError> {
    let quiet = |_: &str| {};
    let loud = |step: &str| eprintln!("· {step}");
    let (state, installed) = crate::run::start(if json { &quiet } else { &loud }).await.map_err(|mut error| {
        if let Some(trouble) = tunnel_trouble() {
            error.next_action = Some(trouble);
        }
        error
    })?;
    let mut lines = vec![if installed {
        "running in the background · starts at login".to_owned()
    } else {
        "running · stops when you log out".to_owned()
    }];
    lines.extend(state.servers.iter().map(|server| format!("{:<12} {}", server.name, server.url)));
    say_all(json, "porchlight is sharing", &lines);

    Ok(())
}

async fn apps(json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;

    if json {
        let apps: Vec<Value> = snap
            .apps
            .iter()
            .map(|app| {
                json!({
                    "name": app.name,
                    "kind": app.kind,
                    "on": app.on,
                    "state": state_word(app.state),
                    "links": app.links,
                    "tools": app.tools,
                    "problem": app.problem,
                })
            })
            .collect();
        out!("{}", json!({ "ok": true, "apps": apps }));
        return Ok(());
    }

    if snap.apps.is_empty() {
        out!("No apps yet. Add one with porchlight apps add, or in porchlight.");
    }

    for app in &snap.apps {
        let links = if app.everywhere {
            "every link".to_owned()
        } else if app.links.is_empty() {
            "not shared".to_owned()
        } else {
            app.links.join(", ")
        };
        let mark = if app.problem.is_some() || app.state == Some(ServerState::Refused) {
            "✗"
        } else if app.on {
            "●"
        } else {
            "○"
        };
        out!("{mark} {:<16} {:<22} {links}{}", app.name, app.kind, if app.on { "" } else { " · off" });

        if let Some(problem) = &app.problem {
            out!("  {problem}");
        }
    }

    Ok(())
}

async fn remove_app(app: &str, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;

    if !snap.app_names().iter().any(|name| name == app) {
        return Err(no_app(app));
    }

    saved(actions::remove_app(&config, &snap.settings, app)).await?;
    say(json, &format!("Removed {app}"));
    Ok(())
}

async fn edit_tool(
    target: &str,
    run: Option<String>,
    description: Option<String>,
    read_only: Option<bool>,
    json: bool,
) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let (app, tool) = target.split_once('/').unwrap_or((target, target));
    let Some(current) = actions::tool_setup(&snap.settings, app, tool) else {
        return Err(if snap.settings.mcp.contains_key(app) {
            ExitError::new(
                ExitCode::NoServer,
                format!("{app} is an MCP server. Only command-line tools can be edited."),
            )
        } else {
            ExitError::new(ExitCode::NoServer, format!("There's no command-line tool called {target}"))
                .next("See them with porchlight apps.")
        });
    };
    let draft = CommandDraft {
        words: command_words(run.as_deref().unwrap_or(&current.run)),
        app: app.to_owned(),
        description: None,
        read_only: false,
        allow_dangerous: false,
    };

    if let Some(problem) = actions::program_problem(&draft.program()) {
        return Err(ExitError::new(ExitCode::NoServer, problem));
    }

    if run.is_some() && draft.runs_client_code() {
        return Err(ExitError::new(ExitCode::NeedsHuman, "This command can run anything a client sends it")
            .next("Name the exact program instead, like backup.sh {folder}."));
    }

    let description = description.or(current.description).unwrap_or_default();
    let read_only = read_only.unwrap_or(current.read_only);
    saved(actions::edit_tool(&config, &snap.settings, app, tool, &draft.words, &description, read_only)).await?;
    say(json, &format!("Saved {target}"));
    Ok(())
}

struct AddDraft {
    name: Option<String>,
    description: Option<String>,
    reads: bool,
    links: Vec<String>,
}

fn checked_name(name: &str, snap: &Snapshot) -> Result<(), ExitError> {
    match actions::name_problem(name, &snap.settings) {
        Some(problem) => Err(ExitError::new(ExitCode::NoServer, problem)),
        None => Ok(()),
    }
}

async fn add_command(draft: AddDraft, run: &str, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let mut command = CommandDraft {
        words: command_words(run),
        app: String::new(),
        description: draft.description,
        read_only: draft.reads,
        allow_dangerous: false,
    };

    if let Some(problem) = actions::program_problem(&command.program()) {
        return Err(ExitError::new(ExitCode::NoServer, problem));
    }

    if command.runs_client_code() {
        return Err(ExitError::new(ExitCode::NeedsHuman, "This command can run anything a client sends it").next(
            "Name the exact program instead, like backup.sh {folder}, or add it in porchlight where you can allow it.",
        ));
    }

    command.app = draft.name.unwrap_or_else(|| command.default_name());
    checked_name(&command.app, &snap)?;

    if let Some(link) = draft.links.iter().find(|link| snap.settings.mcp.contains_key(*link) || !is_app_name(link)) {
        return Err(ExitError::new(ExitCode::NoServer, format!("{link} can't be a link for command-line tools")));
    }

    actions::write_command(&config, &command).map_err(|error| failed(&error))?;
    let latest = config.load().map_err(|error| failed(&error))?.settings;
    actions::set_links_of(&config, &latest, &command.app, &draft.links).map_err(|error| failed(&error))?;
    let (ok, lines) = actions::verdict(&config, &command.app).await;

    if !ok {
        return Err(ExitError::new(ExitCode::ToolFailed, lines.join(" ")));
    }

    say_all(json, &format!("Added {}", command.app), &lines);
    Ok(())
}

async fn add_url(name: Option<String>, url: &str, json: bool) -> Result<(), ExitError> {
    if !is_http_url(url) {
        return Err(ExitError::new(ExitCode::NoServer, format!("{url} isn't an http:// or https:// address")));
    }

    let (config, _, snap) = load().await?;
    let name = name.unwrap_or_else(|| {
        let host = url::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default();
        crate::apps::slugify(&host)
    });
    checked_name(&name, &snap)?;
    let lines = actions::share_mcp(&config, &name, &json!({ "url": url })).await;
    say_all(json, &format!("Added {name}"), &lines);
    Ok(())
}

async fn add_server(name: Option<String>, server: &str, json: bool) -> Result<(), ExitError> {
    let command = command_words(server);

    if let Some(problem) = actions::program_problem(command.first().map(String::as_str).unwrap_or_default()) {
        return Err(ExitError::new(ExitCode::NoServer, problem));
    }

    let (config, _, snap) = load().await?;
    let name = name.unwrap_or_else(|| actions::server_name(&command));
    checked_name(&name, &snap)?;
    let lines = actions::share_mcp(&config, &name, &json!({ "command": actions::run_value(&command) })).await;
    say_all(json, &format!("Added {name}"), &lines);
    Ok(())
}

async fn switch(target: &str, on: bool, json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let word = if on { "on" } else { "off" };
    let (app, tool) = match target.split_once('/') {
        Some((app, tool)) => (app, Some(tool)),
        None => (target, None),
    };

    if !snap.app_names().iter().any(|name| name == app) {
        return Err(no_app(app));
    }

    match tool {
        Some(tool) => saved(actions::set_tool(&config, &format!("{app}/{tool}"), on)).await?,
        None if snap.settings.mcp.contains_key(app) => saved(actions::set_enabled(&config, app, on)).await?,
        None => saved(actions::set_tool(&config, &format!("{app}/*"), on)).await?,
    }

    say(json, &format!("{target} is {word}"));
    Ok(())
}

async fn try_tool(target: &str, inputs: &[String], json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;
    let (app, tool) = target.split_once('/').unwrap_or((target, target));

    let (ok, text) = if snap.settings.commands.contains_key(app) {
        let values = actions::parse_values(&snap.settings, app, tool, &inputs.join(" "));
        let tried = actions::try_tool(&snap.settings, &host_environment(), app, tool, &values).await;
        (tried.ok, tried.text)
    } else if snap.settings.mcp.contains_key(app) {
        let values: serde_json::Map<String, Value> = inputs
            .iter()
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, value)| (name.to_owned(), json!(value)))
            .collect();
        crate::control::try_on_daemon(app, tool, &Value::Object(values))
            .await
            .map_err(|error| ExitError::new(ExitCode::ToolFailed, error))?
    } else {
        return Err(no_app(app));
    };

    if json {
        out!("{}", json!({ "ok": ok, "output": text }));
    } else {
        out!("{}", text.trim_end());
    }

    if ok { Ok(()) } else { Err(ExitError::new(ExitCode::ToolFailed, format!("{target} failed"))) }
}

async fn links(json: bool) -> Result<(), ExitError> {
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
        out!("No links yet. Create one with porchlight links add <name> --app <app>.");
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

async fn link_add(link: &str, apps: &[String], json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;

    if !is_app_name(link) || snap.settings.mcp.contains_key(link) {
        return Err(ExitError::new(ExitCode::NoServer, format!("\"{link}\" can't be a link name"))
            .next("Use lowercase letters, numbers and -, and not the name of an MCP server."));
    }

    if let Some(app) = apps.iter().find(|app| !snap.settings.commands.contains_key(*app)) {
        return Err(if snap.settings.mcp.contains_key(app) {
            ExitError::new(ExitCode::NoServer, format!("{app} is an MCP server, so it has its own link"))
        } else {
            no_app(app)
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

async fn link_remove(link: &str, apps: &[String], json: bool) -> Result<(), ExitError> {
    let (config, _, snap) = load().await?;
    let Some(members) = snap.settings.links.get(link) else {
        return Err(if snap.settings.mcp.contains_key(link) {
            ExitError::new(ExitCode::NoServer, format!("{link} is an MCP server's own link"))
                .next(format!("Remove the app instead with porchlight apps remove {link}."))
        } else {
            no_link(link)
        });
    };

    if apps.is_empty() {
        saved(actions::remove_link(&config, link)).await?;
        say(json, &format!("Deleted {link}. Its clients lost access."));
        return Ok(());
    }

    if members.is_all() {
        return Err(ExitError::new(ExitCode::NoServer, format!("{link} has every command-line tool"))
            .next("Turn an app off with porchlight off <app> instead."));
    }

    if let Some(app) = apps.iter().find(|app| !members.apps().contains(app)) {
        return Err(ExitError::new(ExitCode::NoServer, format!("{app} isn't on {link}")));
    }

    let kept: Vec<String> = members.apps().iter().filter(|app| !apps.contains(app)).cloned().collect();
    saved(actions::set_members(&config, link, &kept)).await?;
    say(
        json,
        &if kept.is_empty() {
            format!("Deleted {link}, it had no apps left")
        } else {
            format!("{link} has {}", kept.join(", "))
        },
    );
    Ok(())
}

async fn clients(json: bool) -> Result<(), ExitError> {
    let (_, _, snap) = load().await?;

    if json {
        let pending: Vec<Value> = snap
            .pending
            .iter()
            .map(|request| {
                json!({ "request": request.id, "client": request.client, "verified": request.verified, "link": request.link, "asked": request.asked })
            })
            .collect();
        let clients: Vec<Value> = snap
            .clients
            .iter()
            .map(|client| {
                json!({ "id": client.id, "name": client.name, "link": client.link, "token": client.token, "last_used": client.last_used })
            })
            .collect();
        out!("{}", json!({ "ok": true, "waiting": pending, "clients": clients }));
        return Ok(());
    }

    for request in &snap.pending {
        let verified = match &request.verified {
            Some(host) if *host == request.client => " ✓".to_owned(),
            Some(host) => format!(" ✓ {host}"),
            None => " (name not verified)".to_owned(),
        };
        out!("! {}{verified} wants {} · {} · {}", request.client, request.link, request.asked, request.id);
    }

    if !snap.pending.is_empty() {
        out!("  approve with porchlight clients approve\n");
    }

    if snap.clients.is_empty() {
        out!("No approved clients yet.");
    }

    for client in &snap.clients {
        out!("● {:<30} {:<12} {:<14} {}", client.name, client.link, client.last_used, client.id);
    }

    Ok(())
}

fn pick_request(snap: &Snapshot, request: Option<String>) -> Result<snapshot::PendingInfo, ExitError> {
    match request {
        Some(id) => snap.pending.iter().find(|pending| pending.id == id).cloned().ok_or_else(|| {
            ExitError::new(ExitCode::NoServer, format!("No waiting request {id}"))
                .next("See them with porchlight clients.")
        }),
        None => match snap.pending.as_slice() {
            [] => Err(ExitError::new(ExitCode::NoServer, "Nothing is waiting")),
            [only] => Ok(only.clone()),
            _ => Err(ExitError::new(ExitCode::NoServer, format!("{} requests are waiting", snap.pending.len()))
                .next("Pick one with --request <id>. See them with porchlight clients.")),
        },
    }
}

async fn client_action(action: ClientsAction, json: bool) -> Result<(), ExitError> {
    let (_, store, snap) = load().await?;

    match action {
        ClientsAction::Approve { request, code } => {
            needs_person("Approving a client")?;
            let pending = pick_request(&snap, request)?;

            if code {
                let code = Auth::new(store).issue_device_code(&pending.id).map_err(|error| failed(&error))?;
                say_all(
                    json,
                    &format!("Code for {} on {}", pending.client, pending.link),
                    &[code, "Enter it on the device that asked. It expires in 5 minutes.".to_owned()],
                );
            } else {
                let port = store.ports().map_err(|error| failed(&error))?.approval;
                system::open_in_browser(&format!("http://127.0.0.1:{port}/approve?req={}", pending.id)).await;
                say(json, &format!("Opened the approval page for {} in your browser", pending.client));
            }
        }
        ClientsAction::Deny { request } => {
            let pending = pick_request(&snap, request)?;
            actions::deny(&store, &pending.id).map_err(|error| failed(&error))?;
            say(json, &format!("Denied {} on {}", pending.client, pending.link));
        }
        ClientsAction::Revoke { everyone: true, .. } => {
            Auth::new(store).revoke_all().map_err(|error| failed(&error))?;
            say(json, "Revoked every client and token");
        }
        ClientsAction::Revoke { client, link, .. } => {
            let wanted = client.unwrap_or_default();
            let matched: Vec<&snapshot::ClientInfo> = snap
                .clients
                .iter()
                .filter(|info| info.id == wanted || info.name.eq_ignore_ascii_case(&wanted))
                .filter(|info| link.as_ref().is_none_or(|link| info.link == *link))
                .collect();

            if matched.is_empty() {
                return Err(ExitError::new(ExitCode::NoServer, format!("No approved client called {wanted}"))
                    .next("See them with porchlight clients."));
            }

            let auth = Auth::new(store);

            for info in &matched {
                auth.revoke_grant(&info.id).map_err(|error| failed(&error))?;
            }

            let links: Vec<&str> = matched.iter().map(|info| info.link.as_str()).collect();
            say(json, &format!("Revoked {wanted} on {}", links.join(", ")));
        }
        ClientsAction::Token { link, name, lasts } => {
            needs_person("Creating a token")?;

            if !snap.links.iter().any(|info| info.name == link) {
                return Err(no_link(&link));
            }

            let ttl = parse_duration(&lasts).ok_or_else(|| {
                ExitError::new(ExitCode::NoServer, format!("\"{lasts}\" isn't a length. Use 30d, 12h or 60m"))
            })?;
            let (id, token) =
                Auth::new(store).create_static_token(&link, &name, ttl).map_err(|error| failed(&error))?;
            say_all(
                json,
                &format!("Token {name} for {link} ({id}) · shown only once"),
                &[token, "Send it as Authorization: Bearer <token>.".to_owned()],
            );
        }
    }

    Ok(())
}

async fn tunnel(json: bool) -> Result<(), ExitError> {
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

async fn use_tunnel(tunnel: &str, json: bool) -> Result<(), ExitError> {
    let tunnel = tunnel.trim_end_matches('/');

    if Provider::named(tunnel).is_none() && !is_https_url(tunnel) {
        return Err(ExitError::new(ExitCode::TunnelFailed, format!("{tunnel} isn't a tunnel porchlight knows"))
            .next("Use opentunnel, or your own tunnel's https:// address."));
    }

    ensure_tunnel(tunnel, json).await?;
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
