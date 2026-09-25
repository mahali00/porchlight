use crate::apps::{apps_from, host_environment};
use crate::config::{Command, CommandApp, Config, GROUP_SETTINGS, McpServer, Settings, decode_command};
use crate::control::{DaemonState, daemon_state};
use crate::core::{Problem, ago, exposed_name, now_ms};
use crate::links::{LinkPlan, ServerState, links_for_app};
use crate::policy::{Rules, rules_from, tool_on};
use crate::serve::ServerView;
use crate::store::{AuditEntry, Outcome, Store};
use crate::system::{ServiceStatus, service_status};
use crate::tunnels::DEFAULT_TUNNEL;
use chrono::{DateTime, Local};
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Changes,
    ReadOnly,
    Resource,
    RunsCode,
    Plain,
}

#[derive(Clone, Debug)]
pub struct ToolRow {
    pub app: String,
    pub tool: String,
    pub exposed: String,
    pub on: bool,
    pub about: String,
    pub badge: Badge,
    pub inputs: Vec<String>,
}

impl ToolRow {
    pub fn rule(&self) -> String {
        format!("{}/{}", self.app, self.tool)
    }
}

#[derive(Clone, Debug)]
pub struct AppGroup {
    pub name: String,
    pub kind: String,
    pub elsewhere: Vec<String>,
    pub everywhere: bool,
    pub tools: Vec<ToolRow>,
}

#[derive(Clone, Debug)]
pub struct LinkInfo {
    pub name: String,
    pub composed: bool,
    pub state: Option<ServerState>,
    pub address: Option<String>,
    pub on: usize,
    pub total: usize,
    pub clients: Vec<String>,
    pub groups: Vec<AppGroup>,
    pub dangerous: Vec<String>,
    pub refused_app: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub name: String,
    pub kind: String,
    pub command: bool,
    pub links: Vec<String>,
    pub everywhere: bool,
    pub on: bool,
    pub state: Option<ServerState>,
    pub detail: String,
    pub problem: Option<String>,
    pub tools: usize,
    pub tool_names: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub link: String,
    pub token: bool,
    pub last_used: String,
    pub since: String,
}

#[derive(Clone, Debug)]
pub struct PendingInfo {
    pub id: String,
    pub client: String,
    pub verified: Option<String>,
    pub link: String,
    pub asked: String,
    pub left: String,
    pub tools: Vec<(String, bool)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Refused,
    Error,
    Change,
    Note,
}

#[derive(Clone, Debug)]
pub struct LogRow {
    pub day: String,
    pub time: String,
    pub client: String,
    pub link: String,
    pub what: String,
    pub result: Verdict,
    pub detail: String,
    pub input: Option<String>,
}

pub struct Snapshot {
    pub settings: Settings,
    pub file_problems: Vec<String>,
    pub live: Option<DaemonState>,
    pub plans: Vec<LinkPlan>,
    pub rules: Rules,
    pub links: Vec<LinkInfo>,
    pub apps: Vec<AppInfo>,
    pub clients: Vec<ClientInfo>,
    pub pending: Vec<PendingInfo>,
    pub log: Vec<LogRow>,
    pub log_today: usize,
    pub service: ServiceStatus,
    pub tunnel: String,
}

fn local_time(at: i64, format: &str) -> String {
    DateTime::from_timestamp_millis(at)
        .map(|at| at.with_timezone(&Local).format(format).to_string())
        .unwrap_or_default()
}

fn day_of(at: i64, now: i64) -> String {
    let day = local_time(at, "%Y-%m-%d");

    if day == local_time(now, "%Y-%m-%d") {
        "today".to_owned()
    } else if day == local_time(now - 86_400_000, "%Y-%m-%d") {
        "yesterday".to_owned()
    } else {
        local_time(at, "%a %b %-d")
    }
}

pub struct ClientNames<'a> {
    store: &'a Store,
    seen: RefCell<HashMap<(String, bool), String>>,
}

impl<'a> ClientNames<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store, seen: RefCell::new(HashMap::new()) }
    }

    fn lookup(&self, id: &str, label: bool) -> String {
        let key = (id.to_owned(), label);

        if let Some(found) = self.seen.borrow().get(&key) {
            return found.clone();
        }

        let found = if label { self.store.client_label(id) } else { self.store.client_name(id) }
            .unwrap_or_else(|_| id.to_owned());
        self.seen.borrow_mut().insert(key, found.clone());
        found
    }

    pub fn name(&self, id: &str) -> String {
        self.lookup(id, false)
    }

    pub fn label(&self, id: &str) -> String {
        self.lookup(id, true)
    }
}

pub fn log_row(entry: &AuditEntry, names: &ClientNames, now: i64) -> LogRow {
    let id = entry.client.clone().unwrap_or_default();
    let client = if id.is_empty() { String::new() } else { names.name(&id) };
    let link = entry.app.clone().unwrap_or_default();
    let subject = entry.subject.clone().unwrap_or_default();
    let took = entry
        .duration_ms
        .map(|ms| if ms < 1000 { format!("{ms} ms") } else { format!("{}.{} s", ms / 1000, ms % 1000 / 100) });
    let base = LogRow {
        day: day_of(entry.at, now),
        time: local_time(entry.at, "%H:%M:%S"),
        client: client.clone(),
        link: link.clone(),
        what: subject.clone(),
        result: Verdict::Note,
        detail: entry.detail.clone(),
        input: entry.input.clone(),
    };
    let change = |what: String| LogRow {
        client: "you".to_owned(),
        what,
        result: Verdict::Change,
        detail: String::new(),
        ..base.clone()
    };

    match entry.event.as_str() {
        "tool.called" => {
            let result = match entry.outcome {
                Some(Outcome::Ok) => Verdict::Ok,
                Some(Outcome::Refused) => Verdict::Refused,
                _ => Verdict::Error,
            };
            let detail = [took, (!entry.detail.is_empty()).then(|| entry.detail.clone())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            LogRow { result, detail, ..base }
        }
        "tool.blocked" => LogRow { result: Verdict::Refused, detail: "tool is off".to_owned(), ..base },
        "method.blocked" => LogRow { result: Verdict::Refused, detail: "not passed on".to_owned(), ..base },
        "auth.rejected" => LogRow {
            client: "unknown".to_owned(),
            what: "connect".to_owned(),
            result: Verdict::Refused,
            detail: "bad token".to_owned(),
            ..base
        },
        "authz.created" => {
            LogRow { what: "asked to connect".to_owned(), result: Verdict::Note, detail: String::new(), ..base }
        }
        "authz.completed" => change(format!("approved {client}")),
        "authz.denied" => change("denied a request".to_owned()),
        "grant.revoked" => change("revoked a client".to_owned()),
        "grant.revoked_all" => change("revoked everyone".to_owned()),
        "grant.revoked_app" => LogRow {
            client: "you".to_owned(),
            what: format!("{link} changed · clients lost access"),
            result: Verdict::Change,
            detail: String::new(),
            ..base
        },
        "token.static_created" => change(format!("created token {}", entry.detail)),
        "token.reuse_detected" => LogRow {
            what: "reused sign-in token".to_owned(),
            result: Verdict::Refused,
            detail: "access revoked".to_owned(),
            ..base
        },
        "config.changed" => LogRow {
            client: "porchlight.json".to_owned(),
            link: "—".to_owned(),
            what: format!("settings changed · {}", entry.detail),
            result: Verdict::Change,
            detail: String::new(),
            ..base
        },
        "dcr.register" => {
            LogRow { client: names.label(&entry.detail), what: "registered".to_owned(), detail: String::new(), ..base }
        }
        "authz.code_issued" => {
            LogRow { client: "you".to_owned(), what: "showed a code".to_owned(), detail: String::new(), ..base }
        }
        "authz.ticket_issued" => LogRow {
            client: "you".to_owned(),
            what: "approved on this computer".to_owned(),
            detail: String::new(),
            ..base
        },
        other => LogRow { what: other.to_owned(), ..base },
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

fn command_of(settings: &Settings, app: &str, tool: &str) -> Option<Command> {
    match settings.commands.get(app)? {
        CommandApp::Single(command) => Some(command.clone()),
        CommandApp::Group(group) => group.members.get(tool).and_then(decode_command),
    }
}

fn configured_tool(settings: &Settings, rules: &Rules, app: &str, tool: &str) -> ToolRow {
    let command = command_of(settings, app, tool);
    let object = match &command {
        Some(Command::Object(object)) => Some(object.as_ref().clone()),
        _ => None,
    };
    let words = match &command {
        Some(Command::Text(text)) => text.clone(),
        Some(Command::Object(object)) => match &object.run {
            crate::config::Words::One(text) => text.clone(),
            crate::config::Words::Many(words) => words.join(" "),
        },
        None => String::new(),
    };
    let badge = if crate::sources::commands::runs_client_code(&crate::apps::template_words(
        &crate::apps::command_words(&words),
    )) {
        Badge::RunsCode
    } else if object.as_ref().is_some_and(|object| object.resource == Some(true)) {
        Badge::Resource
    } else if object.as_ref().is_some_and(|object| object.read_only == Some(true)) {
        Badge::ReadOnly
    } else {
        Badge::Changes
    };

    ToolRow {
        app: app.to_owned(),
        tool: tool.to_owned(),
        exposed: if tool == app { app.to_owned() } else { exposed_name(app, tool) },
        on: tool_on(rules, app, tool),
        about: object.as_ref().and_then(|object| object.description.clone()).unwrap_or_else(|| format!("runs {words}")),
        badge,
        inputs: object
            .and_then(|object| object.inputs)
            .map(|inputs| inputs.keys().cloned().collect())
            .unwrap_or_default(),
    }
}

fn mcp_kind(server: &McpServer) -> String {
    match server {
        McpServer::Http(_) => "MCP server · local URL".to_owned(),
        McpServer::Stdio(_) => "MCP server · command".to_owned(),
    }
}

fn dangerous_named(detail: &str) -> Vec<String> {
    detail
        .strip_prefix("refused: ")
        .and_then(|rest| rest.split_once(" can run"))
        .map(|(names, _)| names.split(", ").map(str::to_owned).collect())
        .unwrap_or_default()
}

pub struct Loaded {
    pub settings: Settings,
    pub config_problem: Option<String>,
    pub problems: Vec<Problem>,
}

fn load_settings(config: &Config) -> Loaded {
    match config.load() {
        Ok(loaded) => Loaded { settings: loaded.settings, config_problem: None, problems: loaded.problems },
        Err(error) => {
            Loaded { settings: Settings::default(), config_problem: Some(error.message), problems: Vec::new() }
        }
    }
}

fn verified_host(client_id: &str) -> Option<String> {
    client_id
        .starts_with("https://")
        .then(|| url::Url::parse(client_id).ok().and_then(|url| url.host_str().map(str::to_owned)))
        .flatten()
}

pub async fn load(config: &Config, store: &Store) -> Snapshot {
    load_with(config, store, None).await
}

pub async fn load_with(config: &Config, store: &Store, service: Option<ServiceStatus>) -> Snapshot {
    let Loaded { settings, config_problem, problems } = load_settings(config);
    let compiled = apps_from(&settings, &host_environment(), None);
    let live = daemon_state().await;
    let rules = rules_from(&settings.tools);
    let now = now_ms();
    let grants = store.grants_list().unwrap_or_default();
    let clients: Vec<ClientInfo> = grants
        .iter()
        .map(|grant| ClientInfo {
            id: grant.id.clone(),
            name: store.client_label(&grant.client_id).unwrap_or_else(|_| grant.client_id.clone()),
            link: grant.server.clone(),
            token: grant.client_id.starts_with("static:"),
            last_used: ago(grant.last_used, now),
            since: local_time(grant.created_at, "%b %-d"),
        })
        .collect();
    let mut snap = Snapshot {
        rules,
        tunnel: settings.tunnel.clone().unwrap_or_else(|| DEFAULT_TUNNEL.to_owned()),
        file_problems: problems
            .iter()
            .filter(|problem| problem.app.is_empty())
            .map(|problem| problem.message.clone())
            .chain(config_problem.clone())
            .collect(),
        live,
        plans: compiled.links,
        links: Vec::new(),
        apps: Vec::new(),
        pending: Vec::new(),
        log: Vec::new(),
        log_today: 0,
        clients,
        service: match service {
            Some(known) => known,
            None => service_status().await,
        },
        settings,
    };
    let app_problems: Vec<Problem> = problems
        .into_iter()
        .chain(compiled.problems)
        .chain(snap.live.iter().flat_map(|live| live.problems.clone()))
        .filter(|problem| !problem.app.is_empty())
        .collect();
    snap.links = snap.plans.iter().map(|plan| snap.link_info(plan, &app_problems)).collect();
    snap.apps = snap.app_names().into_iter().map(|name| snap.app_info(&name, &app_problems)).collect();
    snap.pending = store
        .pending_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.expires_at > now)
        .map(|request| {
            let left = (request.expires_at - now).max(0) / 1000;
            let tools = snap
                .links
                .iter()
                .find(|link| link.name == request.server)
                .map(|link| {
                    link.groups
                        .iter()
                        .flat_map(|group| group.tools.iter())
                        .filter(|tool| tool.on)
                        .map(|tool| (tool.exposed.clone(), matches!(tool.badge, Badge::Changes | Badge::RunsCode)))
                        .collect()
                })
                .unwrap_or_default();
            PendingInfo {
                client: store.client_name(&request.client_id).unwrap_or_else(|_| request.client_id.clone()),
                verified: verified_host(&request.client_id),
                id: request.id,
                link: request.server,
                asked: ago(Some(request.created_at), now).replace("active now", "just now"),
                left: format!("{}:{:02}", left / 60, left % 60),
                tools,
            }
        })
        .collect();
    let entries = store.audit_list(500).unwrap_or_default();
    snap.log_today = entries.iter().filter(|entry| day_of(entry.at, now) == "today").count();
    let names = ClientNames::new(store);
    snap.log = entries.iter().map(|entry| log_row(entry, &names, now)).collect();
    snap
}

impl Snapshot {
    pub fn view(&self, name: &str) -> Option<&ServerView> {
        self.live.as_ref().and_then(|live| live.servers.iter().find(|server| server.name == name))
    }

    pub fn links_of(&self, app: &str) -> Vec<String> {
        links_for_app(&self.plans, app)
    }

    pub fn app_names(&self) -> Vec<String> {
        self.settings.mcp.keys().chain(self.settings.commands.keys()).cloned().collect()
    }

    pub fn on_every_link(&self, app: &str) -> bool {
        self.settings.links.values().any(crate::config::LinkMembers::is_all) && self.settings.commands.contains_key(app)
    }

    fn group_for(&self, link: &str, app: &str, tools: Vec<ToolRow>) -> AppGroup {
        let (kind, command) = match self.settings.mcp.get(app) {
            Some(server) => (mcp_kind(server), false),
            None => ("command".to_owned(), true),
        };
        AppGroup {
            name: app.to_owned(),
            kind,
            elsewhere: if command {
                self.links_of(app).into_iter().filter(|other| other != link).collect()
            } else {
                Vec::new()
            },
            everywhere: command && self.settings.links.get(link).is_some_and(crate::config::LinkMembers::is_all),
            tools,
        }
    }

    fn link_info(&self, plan: &LinkPlan, problems: &[Problem]) -> LinkInfo {
        let name = plan.name().to_owned();
        let view = self.view(&name);
        let composed = matches!(plan, LinkPlan::Commands { .. });
        let members: Vec<String> = plan.members().into_iter().collect();
        let groups: Vec<AppGroup> = match view.filter(|view| !view.catalog.is_empty()) {
            Some(view) => members
                .iter()
                .map(|app| {
                    let tools = view
                        .catalog
                        .iter()
                        .filter(|info| info.app == *app)
                        .map(|info| {
                            let configured = self
                                .settings
                                .commands
                                .contains_key(app)
                                .then(|| configured_tool(&self.settings, &self.rules, app, &info.tool));
                            let badge = if info.dangerous {
                                Badge::RunsCode
                            } else if info.resource {
                                Badge::Resource
                            } else if let Some(configured) = &configured {
                                configured.badge
                            } else if info.read_only == Some(true) {
                                Badge::ReadOnly
                            } else {
                                Badge::Plain
                            };
                            ToolRow {
                                app: app.clone(),
                                tool: info.tool.clone(),
                                exposed: info.name.clone(),
                                on: if composed { tool_on(&self.rules, app, &info.tool) } else { info.on },
                                about: info
                                    .about
                                    .clone()
                                    .or_else(|| configured.as_ref().map(|tool| tool.about.clone()))
                                    .unwrap_or_default(),
                                badge,
                                inputs: configured.map_or_else(|| info.inputs.clone(), |tool| tool.inputs),
                            }
                        })
                        .collect();
                    self.group_for(&name, app, tools)
                })
                .collect(),
            None => members
                .iter()
                .map(|app| {
                    let tools = tool_names(app, self.settings.commands.get(app))
                        .iter()
                        .map(|tool| configured_tool(&self.settings, &self.rules, app, tool))
                        .collect();
                    self.group_for(&name, app, tools)
                })
                .collect(),
        };
        let total = groups.iter().map(|group| group.tools.len()).sum();
        let on = view.map_or_else(
            || groups.iter().flat_map(|group| &group.tools).filter(|tool| tool.on).count(),
            |view| view.tools,
        );
        let source = match plan {
            LinkPlan::Direct { app, .. } => match self.settings.mcp.get(app) {
                Some(McpServer::Http(server)) => format!("MCP server at {}", server.url),
                Some(McpServer::Stdio(server)) => format!(
                    "MCP server started by {}",
                    match &server.command {
                        crate::config::Words::One(text) => text.clone(),
                        crate::config::Words::Many(words) => words.join(" "),
                    }
                ),
                None => String::new(),
            },
            LinkPlan::Commands { .. } => String::new(),
        };

        let refused = match (plan, view) {
            (LinkPlan::Direct { app, .. }, Some(view)) if view.state == ServerState::Refused => {
                (app.clone(), dangerous_named(&view.detail))
            }
            _ => members
                .iter()
                .find_map(|app| {
                    problems
                        .iter()
                        .find(|problem| problem.app == *app && problem.message.starts_with("refused: "))
                        .map(|problem| (app.clone(), dangerous_named(&problem.message)))
                })
                .unwrap_or_default(),
        };

        LinkInfo {
            clients: self
                .clients
                .iter()
                .filter(|client| client.link == name)
                .map(|client| client.name.clone())
                .collect(),
            state: view.map(|view| view.state),
            address: view.map(|view| view.url.clone()),
            dangerous: refused.1,
            refused_app: refused.0,
            name,
            composed,
            on,
            total,
            groups,
            source,
        }
    }

    fn app_info(&self, name: &str, problems: &[Problem]) -> AppInfo {
        let command = self.settings.commands.contains_key(name);
        let names: Vec<String> = if command {
            tool_names(name, self.settings.commands.get(name))
                .iter()
                .map(|tool| crate::core::exposed_name(name, tool))
                .collect()
        } else {
            self.view(name).map(|view| view.catalog.iter().map(|tool| tool.name.clone()).collect()).unwrap_or_default()
        };
        let tools = names.len();
        let kind = match self.settings.mcp.get(name) {
            Some(server) => mcp_kind(server),
            None if tools == 1 => "command".to_owned(),
            None => format!("command · {tools} tools"),
        };
        let on = match self.settings.mcp.get(name) {
            Some(server) => server.enabled(),
            None => crate::policy::app_default_on(&self.rules, name),
        };
        let view = if command { None } else { self.view(name) };

        AppInfo {
            name: name.to_owned(),
            kind,
            command,
            links: self.links_of(name),
            everywhere: self.on_every_link(name),
            on,
            state: view.map(|view| view.state),
            detail: view.map(|view| view.detail.clone()).unwrap_or_default(),
            problem: problems.iter().find(|problem| problem.app == name).map(|problem| problem.message.clone()),
            tools,
            tool_names: names,
        }
    }
}
