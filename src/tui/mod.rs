mod theme;
mod view;

use crate::actions::{self, CommandDraft};
use crate::apps::host_environment;
use crate::auth::Auth;
use crate::config::Config;
use crate::control::{reload_daemon, set_daemon_tunnel};
use crate::core::{is_app_name, is_http_url, is_https_url, now_ms};
use crate::crypto::parse_duration;
use crate::discovery::{DiscoveredServer, discover, is_local_url};
use crate::exit::{ExitCode, ExitError};
use crate::run::ensure_tunnel;
use crate::snapshot::{self, Badge, Snapshot, Verdict};
use crate::store::Store;
use crate::system;
use crate::tunnels::{DEFAULT_TUNNEL, Provider};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Apps,
    Links,
    Clients,
    Logs,
    Tunnel,
}

const TABS: [Tab; 5] = [Tab::Apps, Tab::Links, Tab::Clients, Tab::Logs, Tab::Tunnel];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Self::Links => "Links",
            Self::Apps => "Apps",
            Self::Clients => "Clients",
            Self::Logs => "Logs",
            Self::Tunnel => "Tunnel",
        }
    }

    fn index(self) -> usize {
        TABS.iter().position(|tab| *tab == self).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ListRow {
    Link(String),
    NewLink,
    Unshared(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DetailRow {
    Group { app: String },
    Tool { app: String, tool: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClientRow {
    Pending(String),
    Client(String),
    NewToken,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TunnelRow {
    Open,
    Own,
}

#[derive(Clone)]
struct Check {
    value: String,
    label: String,
    detail: String,
    checked: bool,
    start: bool,
    locked: bool,
    gain: String,
    lose: String,
}

#[derive(Clone)]
struct Choice {
    label: String,
    hint: String,
    value: String,
}

enum Step {
    AddKind,
    Found,
    UrlAddress,
    UrlName { url: String },
    StdioCommand,
    StdioName { command: Vec<String> },
    CommandRun,
    CommandDanger { draft: CommandDraft },
    CommandName { draft: CommandDraft },
    CommandDescription { draft: CommandDraft },
    CommandReadOnly { draft: CommandDraft },
    CommandLinks { draft: CommandDraft },
    EditRun { app: String, tool: String },
    EditAbout { app: String, tool: String, words: Vec<String> },
    EditReads { app: String, tool: String, words: Vec<String>, description: String },
    AppLinks { app: String },
    LinkApps { link: String },
    NewLinkName { app: Option<String> },
    NewLinkApps { name: String },
    DeleteLink { link: String },
    RemoveApp { app: String },
    RemoveTool { app: String, tool: String },
    AllowDangerous { app: String },
    RevokeClient { id: String },
    RevokeAll,
    TokenLink,
    TokenName { link: String },
    TokenExpires { link: String, name: String },
    OwnTunnel,
    Stop,
}

struct Try {
    app: String,
    tool: String,
    about: String,
    changes: bool,
    inputs: Vec<(String, String)>,
    field: usize,
    output: Option<(bool, String)>,
}

enum Mode {
    Normal,
    Busy(String),
    Message {
        title: String,
        lines: Vec<String>,
        danger: bool,
    },
    Confirm {
        title: String,
        lines: Vec<String>,
        typed: Option<String>,
        buffer: String,
        danger: bool,
        step: Step,
    },
    Input {
        title: String,
        context: Vec<(String, String)>,
        prompt: String,
        buffer: String,
        error: Option<String>,
        step: Step,
    },
    Choose {
        title: String,
        context: Vec<(String, String)>,
        question: String,
        options: Vec<Choice>,
        cursor: usize,
        step: Step,
    },
    Checklist {
        title: String,
        lines: Vec<String>,
        items: Vec<Check>,
        cursor: usize,
        action: String,
        note: Option<String>,
        step: Step,
    },
    Code {
        code: String,
        client: String,
        verified: bool,
        until: i64,
        copied: bool,
    },
    Try(Try),
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Search {
    Apps,
    Tools,
    Logs,
}

#[derive(Default)]
struct LogView {
    search: String,
    link: Option<String>,
    client: Option<String>,
    only_changes: bool,
    paused: Option<usize>,
    expanded: bool,
}

struct Job {
    title: String,
    progress: tokio::sync::watch::Receiver<String>,
    handle: tokio::task::JoinHandle<(String, Vec<String>)>,
    since: std::time::Instant,
    hidden: bool,
    hint: Option<String>,
}

enum Answer {
    Text(String),
    Picked(Vec<String>),
    Yes,
}

struct App {
    config: Config,
    store: Store,
    auth: Auth,
    snap: Snapshot,
    tab: Tab,
    pane_detail: bool,
    list_cursor: usize,
    detail_cursor: usize,
    apps_cursor: usize,
    clients_cursor: usize,
    logs_cursor: usize,
    tunnel_cursor: usize,
    folded: HashSet<String>,
    tool_filter: String,
    apps_filter: String,
    logs: LogView,
    first_run_own: bool,
    mode: Mode,
    status: String,
    quit: bool,
    found: Vec<DiscoveredServer>,
    job: Option<Job>,
    searching: Option<Search>,
    service_at: Option<std::time::Instant>,
}

fn message(title: &str, lines: Vec<String>) -> Mode {
    Mode::Message { title: title.to_owned(), lines, danger: false }
}

fn confirm(title: String, lines: Vec<String>, step: Step) -> Mode {
    Mode::Confirm { title, lines, typed: None, buffer: String::new(), danger: false, step }
}

fn typed_confirm(title: String, lines: Vec<String>, word: &str, step: Step) -> Mode {
    Mode::Confirm { title, lines, typed: Some(word.to_owned()), buffer: String::new(), danger: true, step }
}

fn input(title: &str, context: Vec<(String, String)>, prompt: &str, initial: &str, step: Step) -> Mode {
    Mode::Input {
        title: title.to_owned(),
        context,
        prompt: prompt.to_owned(),
        buffer: initial.to_owned(),
        error: None,
        step,
    }
}

fn choice(label: &str, hint: &str, value: &str) -> Choice {
    Choice { label: label.to_owned(), hint: hint.to_owned(), value: value.to_owned() }
}

fn pair(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_owned(), value.into())
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

impl App {
    fn list_rows(&self) -> Vec<ListRow> {
        self.snap
            .links
            .iter()
            .map(|link| ListRow::Link(link.name.clone()))
            .chain(std::iter::once(ListRow::NewLink))
            .chain(
                self.snap
                    .apps
                    .iter()
                    .filter(|app| app.links.is_empty() && !app.everywhere)
                    .map(|app| ListRow::Unshared(app.name.clone())),
            )
            .collect()
    }

    fn selected_link(&self) -> Option<&snapshot::LinkInfo> {
        match self.list_rows().get(self.list_cursor) {
            Some(ListRow::Link(name)) => self.snap.links.iter().find(|link| link.name == *name),
            _ => None,
        }
    }

    fn app_rows(&self) -> Vec<&snapshot::AppInfo> {
        let filter = self.apps_filter.to_lowercase();

        self.snap
            .apps
            .iter()
            .filter(|info| {
                filter.is_empty()
                    || info.name.to_lowercase().contains(&filter)
                    || info.kind.to_lowercase().contains(&filter)
                    || !self.matched_tools(info).is_empty()
            })
            .collect()
    }

    fn matched_tools<'a>(&self, info: &'a snapshot::AppInfo) -> Vec<&'a str> {
        let filter = self.apps_filter.to_lowercase();

        if filter.is_empty() {
            return Vec::new();
        }

        info.tool_names.iter().filter(|name| name.to_lowercase().contains(&filter)).map(String::as_str).collect()
    }

    fn detail_rows(&self) -> Vec<DetailRow> {
        let Some(link) = self.selected_link() else { return Vec::new() };
        let filter = self.tool_filter.to_lowercase();

        link.groups
            .iter()
            .flat_map(|group| {
                let key = format!("{}/{}", link.name, group.name);
                let folded = self.folded.contains(&key) && filter.is_empty();
                let tools: Vec<DetailRow> = group
                    .tools
                    .iter()
                    .filter(|tool| {
                        filter.is_empty()
                            || tool.exposed.to_lowercase().contains(&filter)
                            || tool.about.to_lowercase().contains(&filter)
                    })
                    .filter(|_| !folded)
                    .map(|tool| DetailRow::Tool { app: group.name.clone(), tool: tool.tool.clone() })
                    .collect();
                let header = (link.composed || group.tools.len() > 1 || !filter.is_empty())
                    .then(|| DetailRow::Group { app: group.name.clone() });

                header.into_iter().chain(tools)
            })
            .collect()
    }

    fn tool(&self, app: &str, tool: &str) -> Option<&snapshot::ToolRow> {
        self.selected_link()?.groups.iter().find(|group| group.name == app)?.tools.iter().find(|row| row.tool == tool)
    }

    fn client_rows(&self) -> Vec<ClientRow> {
        self.snap
            .pending
            .iter()
            .map(|pending| ClientRow::Pending(pending.id.clone()))
            .chain(self.snap.clients.iter().map(|client| ClientRow::Client(client.id.clone())))
            .chain(std::iter::once(ClientRow::NewToken))
            .collect()
    }

    fn log_rows(&self) -> Vec<&snapshot::LogRow> {
        let search = self.logs.search.to_lowercase();
        let rows = self.logs.paused.map_or(&self.snap.log[..], |kept| {
            self.snap.log.get(self.snap.log.len().saturating_sub(kept)..).unwrap_or_default()
        });

        rows.iter()
            .filter(|row| self.logs.link.as_ref().is_none_or(|link| row.link == *link))
            .filter(|row| self.logs.client.as_ref().is_none_or(|client| row.client == *client))
            .filter(|row| {
                !self.logs.only_changes || matches!(row.result, Verdict::Refused | Verdict::Change | Verdict::Error)
            })
            .filter(|row| {
                search.is_empty()
                    || [&row.client, &row.link, &row.what, &row.detail]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&search))
            })
            .collect()
    }

    fn tunnel_rows() -> [TunnelRow; 2] {
        [TunnelRow::Open, TunnelRow::Own]
    }

    fn count(&self) -> usize {
        match self.tab {
            Tab::Links if self.pane_detail => self.detail_rows().len(),
            Tab::Links => self.list_rows().len(),
            Tab::Apps => self.app_rows().len(),
            Tab::Clients => self.client_rows().len(),
            Tab::Logs => self.log_rows().len(),
            Tab::Tunnel => 2,
        }
    }

    fn cursor_mut(&mut self) -> &mut usize {
        match self.tab {
            Tab::Links if self.pane_detail => &mut self.detail_cursor,
            Tab::Links => &mut self.list_cursor,
            Tab::Apps => &mut self.apps_cursor,
            Tab::Clients => &mut self.clients_cursor,
            Tab::Logs => &mut self.logs_cursor,
            Tab::Tunnel => &mut self.tunnel_cursor,
        }
    }

    fn clamp(&mut self) {
        let lists = [
            (self.list_rows().len(), 0),
            (self.detail_rows().len(), 1),
            (self.app_rows().len(), 2),
            (self.client_rows().len(), 3),
            (self.log_rows().len(), 4),
        ];

        for (count, which) in lists {
            let cursor = match which {
                0 => &mut self.list_cursor,
                1 => &mut self.detail_cursor,
                2 => &mut self.apps_cursor,
                3 => &mut self.clients_cursor,
                _ => &mut self.logs_cursor,
            };
            *cursor = (*cursor).min(count.saturating_sub(1));
        }
    }

    async fn refresh(&mut self) {
        let fresh = self.service_at.is_some_and(|at| at.elapsed() < Duration::from_secs(30));
        let known = fresh.then(|| self.snap.service.clone());
        self.snap = snapshot::load_with(&self.config, &self.store, known).await;

        if !fresh {
            self.service_at = Some(std::time::Instant::now());
        }

        self.clamp();
    }

    async fn applied(&mut self, result: Result<(), crate::config::ConfigError>, done: String) {
        match result {
            Ok(()) => {
                reload_daemon().await;
                self.status = done;
            }
            Err(error) => self.mode = message("Couldn't change that", vec![error.message]),
        }

        self.refresh().await;
    }

    fn busy(&mut self, terminal: &mut DefaultTerminal, text: &str) {
        self.mode = Mode::Busy(text.to_owned());
        let _ = terminal.draw(|frame| view::draw(self, frame));
    }

    fn run_job<F>(&mut self, title: &str, work: impl FnOnce(tokio::sync::watch::Sender<String>) -> F)
    where
        F: std::future::Future<Output = (String, Vec<String>)> + Send + 'static,
    {
        let (sender, progress) = tokio::sync::watch::channel(String::new());
        self.job = Some(Job {
            title: title.to_owned(),
            progress,
            handle: tokio::spawn(work(sender)),
            since: std::time::Instant::now(),
            hidden: false,
            hint: None,
        });
    }

    async fn finish_job(&mut self, look_for_trouble: bool) {
        if let Some(job) = self.job.as_mut().filter(|job| look_for_trouble && job.since.elapsed().as_secs() >= 15) {
            job.hint = crate::run::tunnel_trouble();
        }

        if !self.job.as_ref().is_some_and(|job| job.handle.is_finished()) {
            return;
        }

        let Some(job) = self.job.take() else { return };
        let (title, lines) =
            job.handle.await.unwrap_or_else(|error| ("Something went wrong".to_owned(), vec![error.to_string()]));
        self.mode = message(&title, lines);
        self.service_at = None;
        self.refresh().await;
    }

    fn start_sharing(&mut self) {
        self.run_job("Starting porchlight", move |progress| async move {
            let say = |text: &str| {
                let _ = progress.send(text.to_owned());
            };
            match crate::run::start(&say).await {
                Ok((state, installed)) => {
                    let link = state.servers.first().map(|server| server.url.clone());
                    let copied = match &link {
                        Some(link) => system::copy_to_clipboard(link).await,
                        None => false,
                    };
                    let mut lines = vec![
                        "✓ tunnel ready".to_owned(),
                        if installed {
                            "✓ running in the background · starts at login, keeps going when you close this".to_owned()
                        } else {
                            "✓ running · stops when you log out".to_owned()
                        },
                        format!("✓ {} shared", plural(state.servers.len(), "link")),
                        String::new(),
                    ];
                    match link {
                        Some(link) => {
                            lines.push(format!("{link}{}", if copied { "  ✓ copied" } else { "" }));
                            lines.push(
                                "Paste it into your client, e.g. Claude → Settings → Connectors → Add.".to_owned(),
                            );
                            lines.push("When it asks to connect, you approve it here.".to_owned());
                        }
                        None => lines.push("Nothing is shared yet. Press + to add an app.".to_owned()),
                    }
                    ("porchlight is sharing".to_owned(), lines)
                }
                Err(error) => (
                    "porchlight isn't sharing yet".to_owned(),
                    [Some(error.message), error.next_action, crate::run::tunnel_trouble()]
                        .into_iter()
                        .flatten()
                        .collect(),
                ),
            }
        });
    }

    fn switch(&mut self, index: usize) {
        self.tab = TABS.get(index % TABS.len()).copied().unwrap_or(Tab::Apps);
        self.status.clear();
    }

    async fn key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        if let Some(job) = self.job.as_mut().filter(|job| !job.hidden) {
            match key.code {
                KeyCode::Esc => {
                    job.hidden = true;
                    self.status = format!("{}… it keeps going in the background", job.title);
                }
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }

        if let Some(target) = self.searching {
            self.search_key(target, key);
            return;
        }

        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(key).await,
            Mode::Busy(text) => self.mode = Mode::Busy(text),
            Mode::Code { code, client, verified, until, .. } if key.code == KeyCode::Char('y') => {
                let copied = system::copy_to_clipboard(&code).await;
                self.mode = Mode::Code { code, client, verified, until, copied };
            }
            Mode::Message { .. } | Mode::Help | Mode::Code { .. } => {}
            Mode::Confirm { title, lines, typed, mut buffer, danger, step } => match (key.code, &typed) {
                (KeyCode::Esc, _) | (KeyCode::Char('n'), None) => {}
                (KeyCode::Enter, Some(word)) if buffer == *word => self.answer(step, Answer::Yes, terminal).await,
                (KeyCode::Enter | KeyCode::Char('y'), None) => self.answer(step, Answer::Yes, terminal).await,
                (KeyCode::Backspace, Some(_)) => {
                    buffer.pop();
                    self.mode = Mode::Confirm { title, lines, typed, buffer, danger, step };
                }
                (KeyCode::Char(c), Some(_)) => {
                    buffer.push(c);
                    self.mode = Mode::Confirm { title, lines, typed, buffer, danger, step };
                }
                _ => self.mode = Mode::Confirm { title, lines, typed, buffer, danger, step },
            },
            Mode::Input { title, context, prompt, mut buffer, error, step } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.answer(step, Answer::Text(buffer), terminal).await,
                KeyCode::Backspace => {
                    buffer.pop();
                    self.mode = Mode::Input { title, context, prompt, buffer, error, step };
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    self.mode = Mode::Input { title, context, prompt, buffer, error, step };
                }
                _ => self.mode = Mode::Input { title, context, prompt, buffer, error, step },
            },
            Mode::Choose { title, context, question, options, cursor, step } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let value = options.get(cursor).map(|option| option.value.clone()).unwrap_or_default();
                    self.answer(step, Answer::Text(value), terminal).await;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mode =
                        Mode::Choose { title, context, question, options, cursor: cursor.saturating_sub(1), step };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = options.len().saturating_sub(1);
                    self.mode =
                        Mode::Choose { title, context, question, options, cursor: (cursor + 1).min(last), step };
                }
                _ => self.mode = Mode::Choose { title, context, question, options, cursor, step },
            },
            Mode::Checklist { title, lines, mut items, cursor, action, note, step } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter
                    if matches!(step, Step::AppLinks { .. } | Step::LinkApps { .. })
                        && items.iter().all(|item| item.checked == item.start) => {}
                KeyCode::Enter => {
                    let picked = items.iter().filter(|item| item.checked).map(|item| item.value.clone()).collect();
                    self.answer(step, Answer::Picked(picked), terminal).await;
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = items.get_mut(cursor).filter(|item| !item.locked) {
                        item.checked = !item.checked;
                    }
                    self.mode = Mode::Checklist { title, lines, items, cursor, action, note, step };
                }
                KeyCode::Char('+') if matches!(step, Step::AppLinks { .. } | Step::CommandLinks { .. }) => {
                    let app = match &step {
                        Step::AppLinks { app } => Some(app.clone()),
                        Step::CommandLinks { draft } => Some(draft.app.clone()),
                        _ => None,
                    };
                    self.mode = input("New link", Vec::new(), "name", "", Step::NewLinkName { app });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mode =
                        Mode::Checklist { title, lines, items, cursor: cursor.saturating_sub(1), action, note, step };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = items.len().saturating_sub(1);
                    self.mode =
                        Mode::Checklist { title, lines, items, cursor: (cursor + 1).min(last), action, note, step };
                }
                _ => self.mode = Mode::Checklist { title, lines, items, cursor, action, note, step },
            },
            Mode::Try(state) => self.try_key(state, key, terminal).await,
        }
    }

    fn search_key(&mut self, target: Search, key: KeyEvent) {
        let text = match target {
            Search::Apps => &mut self.apps_filter,
            Search::Tools => &mut self.tool_filter,
            Search::Logs => &mut self.logs.search,
        };

        match key.code {
            KeyCode::Esc => {
                text.clear();
                self.searching = None;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.searching = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }

        match target {
            Search::Apps => self.apps_cursor = 0,
            Search::Tools => self.detail_cursor = 0,
            Search::Logs => self.logs_cursor = 0,
        }
    }

    async fn normal_key(&mut self, key: KeyEvent) {
        let count = self.count();
        let first_run = self.tab == Tab::Links && self.snap.live.is_none() && self.snap.links.is_empty();

        if first_run && matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')) {
            self.first_run_own = !self.first_run_own;
            return;
        }

        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Esc
                if !self.tool_filter.is_empty() || !self.logs.search.is_empty() || !self.apps_filter.is_empty() =>
            {
                self.tool_filter.clear();
                self.apps_filter.clear();
                self.logs.search.clear();
                return;
            }
            KeyCode::Tab => return self.switch(self.tab.index() + 1),
            KeyCode::BackTab => return self.switch(self.tab.index() + TABS.len() - 1),
            KeyCode::Char(digit @ '1'..='5') => {
                let index = digit.to_digit(10).and_then(|digit| usize::try_from(digit).ok()).unwrap_or(1);
                return self.switch(index - 1);
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let cursor = self.cursor_mut();
                *cursor = cursor.saturating_sub(1);
                self.logs.expanded = false;
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let cursor = self.cursor_mut();
                *cursor = (*cursor + 1).min(count.saturating_sub(1));
                self.logs.expanded = false;
                return;
            }
            KeyCode::Char('g') => {
                *self.cursor_mut() = 0;
                return;
            }
            KeyCode::Char('G') => {
                *self.cursor_mut() = count.saturating_sub(1);
                return;
            }
            KeyCode::Char('+') => return self.add(),
            KeyCode::Char('a') if self.tab != Tab::Clients || self.pending_here().is_none() => {
                if let Some(id) = self.snap.pending.first().map(|pending| pending.id.clone()) {
                    self.approve_in_browser(&id).await;
                }
                return;
            }
            KeyCode::Char('c') if self.tab != Tab::Clients && self.tab != Tab::Logs => {
                if let Some(id) = self.snap.pending.first().map(|pending| pending.id.clone()) {
                    self.show_code(&id);
                }
                return;
            }
            KeyCode::Char('x') if self.tab != Tab::Clients => {
                if let Some(id) = self.snap.pending.first().map(|pending| pending.id.clone()) {
                    self.deny(&id).await;
                }
                return;
            }
            _ => {}
        }

        match self.tab {
            Tab::Links if self.snap.live.is_none() && self.snap.links.is_empty() => {
                self.first_run_key(key);
            }
            Tab::Links => self.links_key(key).await,
            Tab::Apps => self.apps_key(key).await,
            Tab::Clients => self.clients_key(key).await,
            Tab::Logs => self.logs_key(key),
            Tab::Tunnel => self.tunnel_key(key).await,
        }
    }

    fn first_run_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('k' | 'j') => self.first_run_own = !self.first_run_own,
            KeyCode::Enter if self.first_run_own => {
                self.mode = input("Your own https address", Vec::new(), "address", "https://", Step::OwnTunnel);
            }
            KeyCode::Enter => {
                if let Err(error) = actions::set_tunnel(&self.config, DEFAULT_TUNNEL) {
                    self.mode = message("Couldn't save the tunnel", vec![error.message]);
                    return;
                }
                self.start_sharing();
            }
            _ => {}
        }
    }

    fn pending_here(&self) -> Option<String> {
        match self.client_rows().get(self.clients_cursor) {
            Some(ClientRow::Pending(id)) => Some(id.clone()),
            _ => None,
        }
    }

    async fn approve_in_browser(&mut self, id: &str) {
        let port = self.store.ports().map(|ports| ports.approval).unwrap_or_default();
        system::open_in_browser(&format!("http://127.0.0.1:{port}/approve?req={id}")).await;
        "Opened the approval page in your browser".clone_into(&mut self.status);
    }

    fn show_code(&mut self, id: &str) {
        let Some(pending) = self.snap.pending.iter().find(|pending| pending.id == id).cloned() else { return };

        self.mode = match self.auth.issue_device_code(id) {
            Ok(code) => Mode::Code {
                code,
                client: pending.client,
                verified: pending.verified.is_some(),
                until: now_ms() + 5 * 60_000,
                copied: false,
            },
            Err(error) => message("No code", vec![error.to_string()]),
        };
    }

    async fn deny(&mut self, id: &str) {
        self.status = match actions::deny(&self.store, id) {
            Ok(()) => "Denied the request".to_owned(),
            Err(error) => error.message,
        };
        self.refresh().await;
    }

    fn add(&mut self) {
        let found = self.found.len();
        self.mode = Mode::Choose {
            title: "Add".to_owned(),
            context: Vec::new(),
            question: "What do you want to share?".to_owned(),
            options: vec![
                choice(
                    "An MCP server porchlight found",
                    if found > 0 { "from Claude, Cursor, VS Code" } else { "looks in your clients' settings" },
                    "found",
                ),
                choice("An MCP server at an address", "http://127.0.0.1:3000/mcp", "url"),
                choice("An MCP server started by a command", "npx -y @acme/mcp", "stdio"),
                choice("A command-line tool", "battery --json, or a group", "command"),
                choice("A new link", "to put command-line tools on", "link"),
            ],
            cursor: 0,
            step: Step::AddKind,
        };
    }

    async fn links_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.pane_detail = false;
                return;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.pane_detail = !self.detail_rows().is_empty();
                return;
            }
            KeyCode::Char('n') => {
                self.mode = input("New link", Vec::new(), "name", "", Step::NewLinkName { app: None });
                return;
            }
            KeyCode::Char('z') => {
                let Some(link) = self.selected_link() else { return };
                let keys: Vec<String> =
                    link.groups.iter().map(|group| format!("{}/{}", link.name, group.name)).collect();
                if keys.iter().all(|key| self.folded.contains(key)) {
                    for key in &keys {
                        self.folded.remove(key);
                    }
                } else {
                    self.folded.extend(keys);
                }
                self.detail_cursor = 0;
                return;
            }
            KeyCode::Char('/') => {
                self.pane_detail = true;
                self.searching = Some(Search::Tools);
                return;
            }
            KeyCode::Char('y') => {
                if let Some(address) = self.selected_link().and_then(|link| link.address.clone()) {
                    let copied = system::copy_to_clipboard(&address).await;
                    self.status = if copied { format!("Copied {address}") } else { address };
                }
                return;
            }
            KeyCode::Char('e') => {
                if let Some(link) = self.selected_link().filter(|link| link.composed).map(|link| link.name.clone()) {
                    self.link_apps(&link);
                }
                return;
            }
            KeyCode::Char('f') => {
                if let Some(link) = self.selected_link().filter(|link| !link.dangerous.is_empty()).cloned() {
                    let app = link.refused_app.clone();
                    let done = format!("Turned off {} · {app} is shared", link.dangerous.join(", "));
                    self.applied(actions::tools_off(&self.config, &app, &link.dangerous), done).await;
                }
                return;
            }
            KeyCode::Char('D') => {
                if let Some(link) = self.selected_link().filter(|link| !link.dangerous.is_empty()).cloned() {
                    let app = link.refused_app.clone();
                    self.mode = typed_confirm(
                        format!("Allow tools that run code on {app}?"),
                        vec![
                            format!("{} can run commands or code on this computer.", link.dangerous.join(", ")),
                            "Anyone approved for this link could do anything you can.".to_owned(),
                        ],
                        "allow",
                        Step::AllowDangerous { app },
                    );
                }
                return;
            }
            _ => {}
        }

        if self.pane_detail {
            self.detail_key(key).await;
        } else {
            self.list_key(key).await;
        }
    }

    async fn list_key(&mut self, key: KeyEvent) {
        let rows = self.list_rows();
        let Some(row) = rows.get(self.list_cursor).cloned() else { return };

        match (row, key.code) {
            (ListRow::Link(_), KeyCode::Enter) => self.pane_detail = !self.detail_rows().is_empty(),
            (ListRow::NewLink, KeyCode::Enter) => {
                self.mode = input("New link", Vec::new(), "name", "", Step::NewLinkName { app: None });
            }
            (ListRow::Unshared(app), KeyCode::Enter | KeyCode::Char('s')) => self.app_links(&app),
            (ListRow::Link(name), KeyCode::Char('d')) => self.delete_link(&name),
            (ListRow::Unshared(app), KeyCode::Char('d')) => self.remove_app(&app),
            (ListRow::Unshared(app) | ListRow::Link(app), KeyCode::Char(' '))
                if self.snap.settings.mcp.contains_key(&app) =>
            {
                self.toggle_app(&app).await;
            }
            _ => {}
        }
    }

    async fn detail_key(&mut self, key: KeyEvent) {
        let Some(row) = self.detail_rows().get(self.detail_cursor).cloned() else { return };

        match (row, key.code) {
            (DetailRow::Group { app }, KeyCode::Enter) => {
                let key = format!("{}/{app}", self.selected_link().map(|link| link.name.clone()).unwrap_or_default());
                if !self.folded.remove(&key) {
                    self.folded.insert(key);
                }
            }
            (DetailRow::Group { app }, KeyCode::Char(' ')) => self.toggle_app(&app).await,
            (DetailRow::Group { app }, KeyCode::Char('s')) if self.snap.settings.commands.contains_key(&app) => {
                self.app_links(&app);
            }
            (DetailRow::Group { app }, KeyCode::Char('d')) => self.remove_app(&app),
            (DetailRow::Tool { app, tool }, KeyCode::Char(' ') | KeyCode::Enter) => {
                let Some(row) = self.tool(&app, &tool).cloned() else { return };
                let done = format!("{} is {}", row.exposed, if row.on { "off" } else { "on" });
                self.applied(actions::set_tool(&self.config, &row.rule(), !row.on), done).await;
            }
            (DetailRow::Tool { app, tool }, KeyCode::Char('t')) => self.start_try(&app, &tool),
            (DetailRow::Tool { app, tool }, KeyCode::Char('u')) => self.edit_tool(app, tool),
            (DetailRow::Tool { app, tool }, KeyCode::Char('d')) if self.snap.settings.commands.contains_key(&app) => {
                self.mode = Mode::Choose {
                    title: format!("Remove {} from {app}?", crate::core::exposed_name(&app, &tool)),
                    context: Vec::new(),
                    question: "It's deleted from porchlight.json. Clients on its links stop seeing it.".to_owned(),
                    options: vec![
                        choice("Turn it off instead", "keeps the setup, easy to turn back on", "off"),
                        choice("Remove it", "", "remove"),
                        choice("Move it to its own app", "then put that app on other links", "split"),
                    ],
                    cursor: 0,
                    step: Step::RemoveTool { app, tool },
                };
            }
            _ => {}
        }
    }

    fn edit_tool(&mut self, app: String, tool: String) {
        let Some(setup) = actions::tool_setup(&self.snap.settings, &app, &tool) else {
            self.status = format!("Only command-line tools can be edited here. {app} is an MCP server.");
            return;
        };
        let title = format!("Edit {}", crate::core::exposed_name(&app, &tool));
        self.mode = input(&title, Vec::new(), "command", &setup.run, Step::EditRun { app, tool });
    }

    async fn toggle_app(&mut self, app: &str) {
        let settings = self.snap.settings.clone();

        if let Some(server) = settings.mcp.get(app) {
            let on = !server.enabled();
            self.applied(
                actions::set_enabled(&self.config, app, on),
                format!("{app} is {}", if on { "on" } else { "off" }),
            )
            .await;
        } else {
            let on = !crate::policy::app_default_on(&self.snap.rules, app);
            self.applied(
                actions::set_tool(&self.config, &format!("{app}/*"), on),
                format!("{app} is {}", if on { "on" } else { "off" }),
            )
            .await;
        }
    }

    fn remove_app(&mut self, app: &str) {
        let shared_on = self.snap.links_of(app);
        let lines = if shared_on.is_empty() {
            vec!["It isn't on any link.".to_owned()]
        } else {
            vec![format!("Clients on {} lose its tools right away.", shared_on.join(", "))]
        };
        self.mode = confirm(format!("Remove {app}?"), lines, Step::RemoveApp { app: app.to_owned() });
    }

    fn delete_link(&mut self, link: &str) {
        let clients: Vec<String> = self
            .snap
            .clients
            .iter()
            .filter(|client| client.link == link)
            .map(|client| {
                format!(
                    "   {:<20}{} · used {}",
                    client.name,
                    if client.token { "token" } else { "client" },
                    client.last_used
                )
            })
            .collect();
        let members: Vec<String> = self
            .snap
            .links
            .iter()
            .find(|info| info.name == link)
            .map(|info| info.groups.iter().map(|group| group.name.clone()).collect())
            .unwrap_or_default();
        let mut lines = Vec::new();

        if clients.is_empty() {
            lines.push("Nobody is approved on it.".to_owned());
        } else {
            lines.push("These lose access right away:".to_owned());
            lines.extend(clients);
        }

        if self.snap.settings.links.contains_key(link) && !members.is_empty() {
            lines.push(String::new());
            lines.push("The apps stay:".to_owned());
            lines.extend(members.iter().map(|app| {
                let other: Vec<String> = self.snap.links_of(app).into_iter().filter(|other| other != link).collect();
                if other.is_empty() {
                    format!("   {app:<20}moves to \"not shared\"")
                } else {
                    format!("   {app:<20}still on {}", other.join(", "))
                }
            }));
        }

        lines.push(String::new());
        lines.push("The address stops working. A new link with the same name gets new approvals.".to_owned());
        let step = if self.snap.settings.links.contains_key(link) {
            Step::DeleteLink { link: link.to_owned() }
        } else {
            Step::RemoveApp { app: link.to_owned() }
        };
        self.mode = typed_confirm(format!("Delete the {link} link?"), lines, link, step);
    }

    fn clients_of(&self, link: &str) -> Vec<String> {
        self.snap.clients.iter().filter(|client| client.link == link).map(|client| client.name.clone()).collect()
    }

    fn link_checks(&self, app: &str) -> Vec<Check> {
        let current = self.snap.links_of(app);
        let tools = snapshot::tool_names(app, self.snap.settings.commands.get(app));

        self.snap
            .settings
            .links
            .iter()
            .map(|(link, members)| {
                let clients = self.clients_of(link);
                let on = current.contains(link) || members.is_all();
                Check {
                    value: link.clone(),
                    label: link.clone(),
                    detail: match (members.is_all(), clients.is_empty()) {
                        (true, _) => "has every command-line app (*)".to_owned(),
                        (false, true) => "no clients yet".to_owned(),
                        (false, false) => clients.join(", "),
                    },
                    checked: on,
                    start: on,
                    locked: members.is_all(),
                    gain: if clients.is_empty() {
                        "gets it · nobody is approved on it yet".to_owned()
                    } else {
                        format!("{} get {}", clients.join(" and "), tools.join(", "))
                    },
                    lose: if clients.is_empty() {
                        "loses it · nobody is approved on it".to_owned()
                    } else {
                        format!("{} lose {}", clients.join(" and "), tools.join(", "))
                    },
                }
            })
            .collect()
    }

    fn app_links(&mut self, app: &str) {
        let tools = snapshot::tool_names(app, self.snap.settings.commands.get(app));
        let items = self.link_checks(app);

        self.mode = Mode::Checklist {
            title: format!("Links for {app}"),
            lines: vec![format!("{}: {}", plural(tools.len(), "tool"), tools.join(", "))],
            items,
            cursor: 0,
            action: "apply".to_owned(),
            note: Some("Clients stay approved. Connected ones are told right away.".to_owned()),
            step: Step::AppLinks { app: app.to_owned() },
        };
    }

    fn link_apps(&mut self, link: &str) {
        let clients = self.clients_of(link);
        let members = self.snap.settings.links.get(link).cloned();
        let all = members.as_ref().is_some_and(crate::config::LinkMembers::is_all);
        let items = self
            .snap
            .apps
            .iter()
            .filter(|app| app.command)
            .map(|app| {
                let on = all || members.as_ref().is_some_and(|members| members.apps().contains(&app.name));
                let locked = app.everywhere && !all;
                let elsewhere: Vec<String> = app.links.iter().filter(|other| *other != link).cloned().collect();
                let detail = if locked {
                    "on every link (*) · change it from that app".to_owned()
                } else if let Some(problem) = &app.problem {
                    format!("▲ {problem}")
                } else if elsewhere.is_empty() && on {
                    plural(app.tools, "tool")
                } else if elsewhere.is_empty() {
                    format!("{} · not shared yet", plural(app.tools, "tool"))
                } else {
                    format!("{} · also on {}", plural(app.tools, "tool"), elsewhere.join(", "))
                };
                let tools = snapshot::tool_names(&app.name, self.snap.settings.commands.get(&app.name));
                let who = clients.join(" and ");
                Check {
                    value: app.name.clone(),
                    label: app.name.clone(),
                    detail,
                    checked: on || locked,
                    start: on || locked,
                    locked,
                    gain: if clients.is_empty() {
                        "goes on it · nobody is approved on it yet".to_owned()
                    } else {
                        format!("{who} get {}", tools.join(", "))
                    },
                    lose: if clients.is_empty() {
                        "comes off it · nobody is approved on it".to_owned()
                    } else {
                        format!("{who} lose {}", tools.join(", "))
                    },
                }
            })
            .collect();

        self.mode = Mode::Checklist {
            title: format!("Apps on {link}"),
            lines: vec![if clients.is_empty() {
                "No clients yet.".to_owned()
            } else {
                format!("{}: {}", plural(clients.len(), "client"), clients.join(", "))
            }],
            items,
            cursor: 0,
            action: "apply".to_owned(),
            note: Some("MCP servers have their own links and can't be added here.".to_owned()),
            step: Step::LinkApps { link: link.to_owned() },
        };
    }

    async fn apps_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('/') {
            self.searching = Some(Search::Apps);
            return;
        }

        let Some(app) = self.app_rows().get(self.apps_cursor).copied().cloned() else { return };

        match key.code {
            KeyCode::Enter => {
                if let Some(index) = self
                    .list_rows()
                    .iter()
                    .position(|row| matches!(row, ListRow::Link(name) if app.links.contains(name)))
                {
                    self.tab = Tab::Links;
                    self.list_cursor = index;
                    self.pane_detail = true;
                    self.detail_cursor = 0;
                } else if app.command {
                    self.app_links(&app.name);
                }
            }
            KeyCode::Char(' ') => self.toggle_app(&app.name).await,
            KeyCode::Char('s') if app.command => self.app_links(&app.name),
            KeyCode::Char('d') => self.remove_app(&app.name),
            KeyCode::Char('t') if app.command => {
                let tools = snapshot::tool_names(&app.name, self.snap.settings.commands.get(&app.name));
                if let Some(tool) = tools.first() {
                    self.try_tool(&app.name, tool);
                }
            }
            KeyCode::Char('t') => {
                self.status = format!("{} has {} tools. Press enter, then t on the one to try", app.name, app.tools);
            }
            KeyCode::Char('o') => {
                system::open_in_browser(&self.config.path.display().to_string()).await;
                "Opened porchlight.json".clone_into(&mut self.status);
            }
            _ => {}
        }
    }

    async fn clients_key(&mut self, key: KeyEvent) {
        let Some(row) = self.client_rows().get(self.clients_cursor).cloned() else { return };

        match (row, key.code) {
            (ClientRow::Pending(id), KeyCode::Enter | KeyCode::Char('a')) => self.approve_in_browser(&id).await,
            (ClientRow::Pending(id), KeyCode::Char('c')) => self.show_code(&id),
            (ClientRow::Pending(id), KeyCode::Char('x')) => self.deny(&id).await,
            (ClientRow::Client(id), KeyCode::Char('r')) => {
                let Some(client) = self.snap.clients.iter().find(|client| client.id == id).cloned() else { return };
                self.mode = confirm(
                    format!("Revoke {}?", client.name),
                    vec![format!("It loses {} right away. It can ask again later.", client.link)],
                    Step::RevokeClient { id },
                );
            }
            (ClientRow::Client(id), KeyCode::Char('l')) => {
                self.logs.client =
                    self.snap.clients.iter().find(|client| client.id == id).map(|client| client.name.clone());
                self.tab = Tab::Logs;
                self.logs_cursor = 0;
            }
            (_, KeyCode::Char('R')) => {
                self.mode = typed_confirm(
                    "Revoke everyone?".to_owned(),
                    vec![format!("{} lose access right away.", plural(self.snap.clients.len(), "client and token"))],
                    "everyone",
                    Step::RevokeAll,
                );
            }
            (_, KeyCode::Char('n')) | (ClientRow::NewToken, KeyCode::Enter) => self.new_token(),
            _ => {}
        }
    }

    fn new_token(&mut self) {
        let options: Vec<Choice> = self.snap.links.iter().map(|link| choice(&link.name, "", &link.name)).collect();

        self.mode = if options.is_empty() {
            message("No links yet", vec!["Share an app first, then create a token for its link.".to_owned()])
        } else {
            Mode::Choose {
                title: "New token".to_owned(),
                context: Vec::new(),
                question: "Which link is the token for?".to_owned(),
                options,
                cursor: 0,
                step: Step::TokenLink,
            }
        };
    }

    fn logs_key(&mut self, key: KeyEvent) {
        let links: Vec<String> = self.snap.links.iter().map(|link| link.name.clone()).collect();
        let mut clients: Vec<String> =
            self.snap.log.iter().map(|row| row.client.clone()).filter(|client| !client.is_empty()).collect();
        clients.sort();
        clients.dedup();
        let next = |list: &[String], current: &Option<String>| -> Option<String> {
            match current {
                None => list.first().cloned(),
                Some(value) => list.iter().skip_while(|item| *item != value).nth(1).cloned(),
            }
        };

        match key.code {
            KeyCode::Enter => self.logs.expanded = !self.logs.expanded,
            KeyCode::Char('/') => {
                self.searching = Some(Search::Logs);
            }
            KeyCode::Char('l') => {
                self.logs.link = next(&links, &self.logs.link);
                self.logs_cursor = 0;
            }
            KeyCode::Char('c') => {
                self.logs.client = next(&clients, &self.logs.client);
                self.logs_cursor = 0;
            }
            KeyCode::Char('o') => {
                self.logs.only_changes = !self.logs.only_changes;
                self.logs_cursor = 0;
            }
            KeyCode::Char('p') => {
                self.logs.paused = match self.logs.paused {
                    Some(_) => None,
                    None => Some(self.snap.log.len()),
                };
            }
            _ => {}
        }
    }

    async fn tunnel_key(&mut self, key: KeyEvent) {
        let row = Self::tunnel_rows().get(self.tunnel_cursor).copied();

        match (row, key.code) {
            (Some(TunnelRow::Open), KeyCode::Enter) => self.use_tunnel(DEFAULT_TUNNEL.to_owned()),
            (Some(TunnelRow::Own), KeyCode::Enter) => {
                self.mode = input("Your own https address", Vec::new(), "address", "https://", Step::OwnTunnel);
            }
            (_, KeyCode::Char('y')) => {
                if let Some(address) = self.snap.live.as_ref().map(|live| live.public_url.clone()) {
                    let copied = system::copy_to_clipboard(&address).await;
                    self.status = if copied { format!("Copied {address}") } else { address };
                }
            }
            (_, KeyCode::Char('w')) => {
                let port = self.store.ports().map(|ports| ports.approval).unwrap_or_default();
                system::open_in_browser(&format!("http://127.0.0.1:{port}/")).await;
                "Opened the local page".clone_into(&mut self.status);
            }
            (_, KeyCode::Char('o')) => {
                system::open_in_browser(&self.config.path.display().to_string()).await;
                "Opened porchlight.json".clone_into(&mut self.status);
            }
            (_, KeyCode::Char('S')) if self.snap.live.is_some() => {
                self.mode = confirm(
                    "Stop sharing?".to_owned(),
                    vec![
                        "porchlight stops and its background service is removed.".to_owned(),
                        "Approved clients can reconnect when you share again.".to_owned(),
                    ],
                    Step::Stop,
                );
            }
            (_, KeyCode::Char('S')) => self.start_sharing(),
            _ => {}
        }
    }

    fn start_try(&mut self, app: &str, tool: &str) {
        let read_only = self.tool(app, tool).is_some_and(|row| matches!(row.badge, Badge::ReadOnly));

        if self.snap.settings.commands.contains_key(app) || read_only {
            self.try_tool(app, tool);
        } else if self.snap.live.is_none() {
            "Start porchlight to try MCP tools".clone_into(&mut self.status);
        } else {
            self.status = format!("{tool} may change things, so it only runs when a client calls it");
        }
    }

    fn try_tool(&mut self, app: &str, tool: &str) {
        let settings = &self.snap.settings;
        let Some(row) = self
            .snap
            .links
            .iter()
            .flat_map(|link| link.groups.iter())
            .flat_map(|group| group.tools.iter())
            .find(|row| row.app == app && row.tool == tool)
            .cloned()
            .or_else(|| {
                settings.commands.contains_key(app).then(|| snapshot::ToolRow {
                    app: app.to_owned(),
                    tool: tool.to_owned(),
                    exposed: tool.to_owned(),
                    on: true,
                    about: String::new(),
                    badge: Badge::Changes,
                    inputs: Vec::new(),
                })
            })
        else {
            return;
        };

        self.mode = Mode::Try(Try {
            app: app.to_owned(),
            tool: tool.to_owned(),
            about: row.about,
            changes: matches!(row.badge, Badge::Changes | Badge::RunsCode),
            inputs: row.inputs.into_iter().map(|name| (name, String::new())).collect(),
            field: 0,
            output: None,
        });
    }

    async fn try_key(&mut self, mut state: Try, key: KeyEvent, terminal: &mut DefaultTerminal) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab | KeyCode::Down => state.field = (state.field + 1) % state.inputs.len().max(1),
            KeyCode::BackTab | KeyCode::Up => {
                state.field = (state.field + state.inputs.len().max(1) - 1) % state.inputs.len().max(1);
            }
            KeyCode::Backspace => {
                if let Some((_, value)) = state.inputs.get_mut(state.field) {
                    value.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some((_, value)) = state.inputs.get_mut(state.field) {
                    value.push(c);
                }
            }
            KeyCode::Enter => {
                let text =
                    state.inputs.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join(" ");
                self.mode = Mode::Try(state);
                let _ = terminal.draw(|frame| view::draw(self, frame));
                let Mode::Try(mut state) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
                let started = std::time::Instant::now();
                let (ok, output) = if self.snap.settings.commands.contains_key(&state.app) {
                    let values = actions::parse_values(&self.snap.settings, &state.app, &state.tool, &text);
                    let tried =
                        actions::try_tool(&self.snap.settings, &host_environment(), &state.app, &state.tool, &values)
                            .await;
                    (tried.ok, tried.text)
                } else {
                    let values: serde_json::Map<String, serde_json::Value> =
                        state.inputs.iter().map(|(name, value)| (name.clone(), json!(value))).collect();
                    crate::control::try_on_daemon(&state.app, &state.tool, &serde_json::Value::Object(values))
                        .await
                        .unwrap_or_else(|error| (false, error))
                };
                let took = started.elapsed().as_millis();
                state.output = Some((ok, format!("{took} ms\n{output}")));
                self.mode = Mode::Try(state);
                return;
            }
            _ => {}
        }

        self.mode = Mode::Try(state);
    }

    fn use_tunnel(&mut self, tunnel: String) {
        let config = self.config.clone();
        let running = self.snap.live.is_some();
        self.run_job("Switching the tunnel", move |progress| async move {
            let say = |text: &str| {
                let _ = progress.send(text.to_owned());
            };

            if Provider::named(&tunnel).is_some() {
                say("Installing OpenTunnel if it's missing");

                if let Err(error) = ensure_tunnel(&tunnel, true).await {
                    return (
                        "Tunnel isn't installed".to_owned(),
                        [Some(error.message), error.next_action].into_iter().flatten().collect(),
                    );
                }
            }

            if let Err(error) = actions::set_tunnel(&config, &tunnel) {
                return ("Couldn't save the tunnel".to_owned(), vec![error.message]);
            }

            if !running {
                return ("Tunnel saved".to_owned(), vec!["porchlight uses it the next time it starts.".to_owned()]);
            }

            say("Opening the new tunnel");
            match set_daemon_tunnel(&tunnel).await {
                Ok(state) => {
                    ("Tunnel changed".to_owned(), vec![format!("Clients reach porchlight at {}", state.public_url)])
                }
                Err(error) => (
                    "The tunnel was saved but didn't open".to_owned(),
                    [Some(error), crate::run::tunnel_trouble()].into_iter().flatten().collect(),
                ),
            }
        });
    }

    fn draft_context(draft: &CommandDraft) -> Vec<(String, String)> {
        let mut context = vec![pair("kind", "command-line tool"), pair("command", draft.shown())];

        if !draft.app.is_empty() {
            context.push(pair("name", draft.app.clone()));
        }

        if let Some(description) = &draft.description {
            context.push(pair("what it does", description.clone()));
        }

        context
    }

    async fn answer(&mut self, step: Step, answer: Answer, terminal: &mut DefaultTerminal) {
        self.mode = Mode::Normal;
        let settings = self.snap.settings.clone();
        let text = match &answer {
            Answer::Text(text) => text.trim().to_owned(),
            Answer::Picked(_) | Answer::Yes => String::new(),
        };
        let picked = match answer {
            Answer::Picked(picked) => picked,
            _ => Vec::new(),
        };

        match step {
            Step::AddKind => match text.as_str() {
                "command" => {
                    self.mode =
                        input("Add an app", vec![pair("kind", "command-line tool")], "command", "", Step::CommandRun);
                }
                "found" => self.find_servers(&settings, terminal).await,
                "url" => {
                    self.mode = input(
                        "Add › MCP server at an address",
                        Vec::new(),
                        "address",
                        "http://127.0.0.1:",
                        Step::UrlAddress,
                    );
                }
                "link" => self.mode = input("New link", Vec::new(), "name", "", Step::NewLinkName { app: None }),
                _ => {
                    self.mode = input(
                        "Add › MCP server started by a command",
                        Vec::new(),
                        "command",
                        "npx -y ",
                        Step::StdioCommand,
                    );
                }
            },
            Step::Found => {
                let chosen: Vec<DiscoveredServer> =
                    self.found.iter().filter(|server| picked.contains(&server.name)).cloned().collect();
                let mut lines = Vec::new();

                for found in chosen {
                    let Some(entry) = found.entry() else { continue };
                    let latest = self.config.load().map(|loaded| loaded.settings).unwrap_or_default();
                    let name = if latest.mcp.contains_key(&found.name)
                        || latest.commands.contains_key(&found.name)
                        || !is_app_name(&found.name)
                    {
                        crate::apps::slugify(&found.name)
                    } else {
                        found.name.clone()
                    };
                    self.busy(terminal, &format!("Sharing {name}…"));
                    lines.extend(
                        actions::share_mcp(&self.config, &name, &serde_json::to_value(&entry).unwrap_or_default())
                            .await,
                    );
                }

                self.mode = message("Shared", lines);
                self.refresh().await;
            }
            Step::UrlAddress => {
                if !is_http_url(&text) {
                    self.mode = Mode::Input {
                        title: "Add › MCP server at an address".to_owned(),
                        context: Vec::new(),
                        prompt: "address".to_owned(),
                        buffer: text,
                        error: Some("That isn't an address. It starts with http:// or https://".to_owned()),
                        step: Step::UrlAddress,
                    };
                    return;
                }

                if !is_local_url(&text) {
                    self.mode = Mode::Input {
                        title: "Add › MCP server at an address".to_owned(),
                        context: Vec::new(),
                        prompt: "address".to_owned(),
                        buffer: text,
                        error: Some("That's not on this computer. porchlight only shares servers running here. Add a remote server to your client directly; it doesn't need porchlight.".to_owned()),
                        step: Step::UrlAddress,
                    };
                    return;
                }

                let suggested = url::Url::parse(&text)
                    .map(|url| {
                        crate::apps::slugify(&format!(
                            "{}-{}",
                            url.host_str().unwrap_or_default(),
                            url.port().unwrap_or_default()
                        ))
                    })
                    .unwrap_or_default();
                self.mode = input(
                    "Add › MCP server at an address",
                    vec![pair("address", text.clone())],
                    "name",
                    &suggested,
                    Step::UrlName { url: text },
                );
            }
            Step::UrlName { url } => {
                if let Some(problem) = actions::name_problem(&text, &settings) {
                    self.mode = Mode::Input {
                        title: "Add › MCP server at an address".to_owned(),
                        context: vec![pair("address", url.clone())],
                        prompt: "name".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::UrlName { url },
                    };
                    return;
                }

                self.busy(terminal, &format!("Sharing {text}…"));
                let lines = actions::share_mcp(&self.config, &text, &json!({ "url": url })).await;
                self.mode = message("Shared", lines);
                self.refresh().await;
            }
            Step::StdioCommand => {
                let command = crate::apps::command_words(&text);

                if let Some(problem) = actions::program_problem(command.first().map(String::as_str).unwrap_or_default())
                {
                    self.mode = Mode::Input {
                        title: "Add › MCP server started by a command".to_owned(),
                        context: Vec::new(),
                        prompt: "command".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::StdioCommand,
                    };
                    return;
                }

                let suggested = actions::server_name(&command);
                self.mode = input(
                    "Add › MCP server started by a command",
                    vec![pair("command", text.clone())],
                    "name",
                    &suggested,
                    Step::StdioName { command },
                );
            }
            Step::StdioName { command } => {
                if let Some(problem) = actions::name_problem(&text, &settings) {
                    self.mode = Mode::Input {
                        title: "Add › MCP server started by a command".to_owned(),
                        context: vec![pair("command", command.join(" "))],
                        prompt: "name".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::StdioName { command },
                    };
                    return;
                }

                self.busy(terminal, &format!("Sharing {text}…"));
                let lines =
                    actions::share_mcp(&self.config, &text, &json!({ "command": actions::run_value(&command) })).await;
                self.mode = message("Shared", lines);
                self.refresh().await;
            }
            Step::CommandRun => {
                let draft = CommandDraft {
                    words: crate::apps::command_words(&text),
                    app: String::new(),
                    description: None,
                    read_only: false,
                    allow_dangerous: false,
                };

                if let Some(problem) = actions::program_problem(&draft.program()) {
                    self.mode = Mode::Input {
                        title: "Add an app".to_owned(),
                        context: vec![pair("kind", "command-line tool")],
                        prompt: "command".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::CommandRun,
                    };
                    return;
                }

                self.mode = if draft.runs_client_code() {
                    typed_confirm(
                        "▲ This command can run anything".to_owned(),
                        vec![
                            format!("command   {}", draft.shown()),
                            String::new(),
                            "Its input comes from the client and goes straight to a shell or".to_owned(),
                            "interpreter. A client on its link could read your files, delete them,".to_owned(),
                            "or install things, as you.".to_owned(),
                            String::new(),
                            "Safer: name the exact program, like  backup.sh {folder}".to_owned(),
                            "esc changes the command. To keep it, allow dangerous tools for this app.".to_owned(),
                        ],
                        "allow",
                        Step::CommandDanger { draft },
                    )
                } else {
                    let name = draft.default_name();
                    let context = Self::draft_context(&draft);
                    input("Add an app", context, "name", &name, Step::CommandName { draft })
                };
            }
            Step::CommandDanger { mut draft } => {
                draft.allow_dangerous = true;
                let name = draft.default_name();
                let context = Self::draft_context(&draft);
                self.mode = input("Add an app", context, "name", &name, Step::CommandName { draft });
            }
            Step::CommandName { mut draft } => {
                if let Some(problem) = actions::name_problem(&text, &settings) {
                    let context = Self::draft_context(&draft);
                    self.mode = Mode::Input {
                        title: "Add an app".to_owned(),
                        context,
                        prompt: "name".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::CommandName { draft },
                    };
                    return;
                }

                draft.app = text;
                let context = Self::draft_context(&draft);
                self.mode = input("Add an app", context, "what it does", "", Step::CommandDescription { draft });
            }
            Step::CommandDescription { mut draft } => {
                draft.description = (!text.is_empty()).then_some(text);
                let context = Self::draft_context(&draft);
                self.mode = Mode::Choose {
                    title: "Add an app".to_owned(),
                    context,
                    question: "Does it change anything?".to_owned(),
                    options: vec![
                        choice("No, it only reads", "porchlight runs it once to check", "reads"),
                        choice("Yes, it changes things", "never run until a client calls it", "changes"),
                    ],
                    cursor: 0,
                    step: Step::CommandReadOnly { draft },
                };
            }
            Step::CommandReadOnly { mut draft } => {
                draft.read_only = text == "reads";
                let mut context = Self::draft_context(&draft);
                context.push(pair("changes things", if draft.read_only { "no, it only reads" } else { "yes" }));

                if draft.read_only && !draft.takes_inputs() {
                    self.busy(terminal, &format!("Running {} once…", draft.shown()));
                    let tried = actions::try_draft(&draft).await;
                    let first = tried.text.lines().next().unwrap_or_default().chars().take(70).collect::<String>();
                    context.push(pair("ran it once", format!("{} {first}", if tried.ok { "✓" } else { "✗" })));
                }

                let items = self.link_checks(&draft.app);
                self.mode = Mode::Checklist {
                    title: format!("Add an app › {}", draft.app),
                    lines: context.iter().map(|(label, value)| format!("{label:<16}{value}")).chain([String::new(), format!("Which links should have {}?", draft.app)]).collect(),
                    items,
                    cursor: 0,
                    action: "add".to_owned(),
                    note: Some("MCP servers have their own links, so they aren't listed. Tick none to keep it under \"not shared\".".to_owned()),
                    step: Step::CommandLinks { draft },
                };
            }
            Step::EditRun { app, tool } => {
                let words = crate::apps::command_words(&text);
                let title = format!("Edit {}", crate::core::exposed_name(&app, &tool));
                let draft = CommandDraft {
                    words: words.clone(),
                    app: app.clone(),
                    description: None,
                    read_only: false,
                    allow_dangerous: false,
                };
                let problem = actions::program_problem(&draft.program()).or_else(|| {
                    draft
                        .runs_client_code()
                        .then(|| "This hands client input to a shell. Name the exact program instead".to_owned())
                });

                if let Some(problem) = problem {
                    self.mode = Mode::Input {
                        title,
                        context: Vec::new(),
                        prompt: "command".to_owned(),
                        buffer: text,
                        error: Some(problem),
                        step: Step::EditRun { app, tool },
                    };
                    return;
                }

                let about =
                    actions::tool_setup(&settings, &app, &tool).and_then(|setup| setup.description).unwrap_or_default();
                self.mode = input(
                    &title,
                    vec![pair("command", draft.shown())],
                    "what it does",
                    &about,
                    Step::EditAbout { app, tool, words },
                );
            }
            Step::EditAbout { app, tool, words } => {
                let reads = actions::tool_setup(&settings, &app, &tool).is_some_and(|setup| setup.read_only);
                let mut options =
                    vec![choice("No, it only reads", "", "reads"), choice("Yes, it changes things", "", "changes")];

                if !reads {
                    options.reverse();
                }

                self.mode = Mode::Choose {
                    title: format!("Edit {}", crate::core::exposed_name(&app, &tool)),
                    context: vec![pair("command", words.join(" ")), pair("what it does", text.clone())],
                    question: "Does it change anything?".to_owned(),
                    options,
                    cursor: 0,
                    step: Step::EditReads { app, tool, words, description: text },
                };
            }
            Step::EditReads { app, tool, words, description } => {
                let done = format!("Saved {}", crate::core::exposed_name(&app, &tool));
                self.applied(
                    actions::edit_tool(&self.config, &settings, &app, &tool, &words, &description, text == "reads"),
                    done,
                )
                .await;
            }
            Step::CommandLinks { draft } => {
                let written = actions::write_command(&self.config, &draft).and_then(|()| {
                    let latest = self.config.load()?.settings;
                    actions::set_links_of(&self.config, &latest, &draft.app, &picked)
                });

                if let Err(error) = written {
                    self.mode = message("Couldn't add it", vec![error.message]);
                    return;
                }

                self.busy(terminal, &format!("Sharing {}…", draft.app));
                let (ok, lines) = actions::verdict(&self.config, &draft.app).await;
                self.mode = message(if ok { "Added" } else { "Added, but not shared yet" }, lines);
                self.refresh().await;
            }
            Step::AppLinks { app } => {
                let done = if picked.is_empty() {
                    format!("{app} isn't on any link")
                } else {
                    format!("{app} is on {}", picked.join(", "))
                };
                self.applied(actions::set_links_of(&self.config, &settings, &app, &picked), done).await;
            }
            Step::LinkApps { link } => {
                let apps: Vec<String> = picked
                    .into_iter()
                    .filter(|app| {
                        !self.snap.on_every_link(app)
                            || settings.links.get(&link).is_some_and(crate::config::LinkMembers::is_all)
                    })
                    .collect();
                let done = format!("{link} has {}", plural(apps.len(), "app"));
                let result = if settings.links.get(&link).is_some_and(crate::config::LinkMembers::is_all) {
                    actions::set_members(&self.config, &link, &apps)
                } else {
                    actions::new_link(&self.config, &link, &apps)
                };
                self.applied(result, done).await;
            }
            Step::NewLinkName { app } => {
                if !is_app_name(&text) || settings.links.contains_key(&text) || settings.mcp.contains_key(&text) {
                    self.mode = Mode::Input {
                        title: "New link".to_owned(),
                        context: Vec::new(),
                        prompt: "name".to_owned(),
                        buffer: text,
                        error: Some(
                            "Use lowercase letters, numbers and -, and a name no other link or app uses".to_owned(),
                        ),
                        step: Step::NewLinkName { app },
                    };
                    return;
                }

                let address =
                    self.snap.live.as_ref().map(|live| format!("{}{}", live.public_url, crate::core::link_path(&text)));
                let items = self
                    .snap
                    .apps
                    .iter()
                    .filter(|info| info.command)
                    .map(|info| Check {
                        value: info.name.clone(),
                        label: info.name.clone(),
                        detail: if info.everywhere {
                            "on every link (*)".to_owned()
                        } else {
                            plural(info.tools, "tool")
                        },
                        checked: info.everywhere || app.as_ref() == Some(&info.name),
                        start: info.everywhere,
                        locked: info.everywhere,
                        gain: "goes on it".to_owned(),
                        lose: String::new(),
                    })
                    .collect();
                self.mode = Mode::Checklist {
                    title: "New link".to_owned(),
                    lines: vec![
                        format!("name     {text}"),
                        format!(
                            "address  {}",
                            address.unwrap_or_else(|| format!("…{}", crate::core::link_path(&text)))
                        ),
                        String::new(),
                        "Which apps go on it?".to_owned(),
                    ],
                    items,
                    cursor: 0,
                    action: "create and copy the address".to_owned(),
                    note: Some("Nobody can use it yet. Share the address; you approve whoever asks.".to_owned()),
                    step: Step::NewLinkApps { name: text },
                };
            }
            Step::NewLinkApps { name } => {
                let apps: Vec<String> = picked.into_iter().filter(|app| !self.snap.on_every_link(app)).collect();
                let result = actions::new_link(&self.config, &name, &apps);
                self.applied(result, format!("Created {name}")).await;
                if let Some(address) =
                    self.snap.links.iter().find(|link| link.name == name).and_then(|link| link.address.clone())
                    && system::copy_to_clipboard(&address).await
                {
                    self.status = format!("Created {name} · copied {address}");
                }
            }
            Step::DeleteLink { link } => {
                let revoked = self
                    .auth
                    .revoke_server(&link)
                    .map_err(|error| crate::config::ConfigError { message: error.to_string() });
                let result = revoked.and_then(|()| actions::remove_link(&self.config, &link));
                self.applied(result, format!("Deleted {link}")).await;
                self.list_cursor = 0;
            }
            Step::RemoveApp { app } => {
                self.applied(
                    actions::remove_app(&self.config, &settings, &app),
                    format!("Removed {app}. Clients can't use it anymore"),
                )
                .await;
            }
            Step::RemoveTool { app, tool } => match text.as_str() {
                "off" => {
                    self.applied(
                        actions::set_tool(&self.config, &format!("{app}/{tool}"), false),
                        format!("{tool} is off"),
                    )
                    .await;
                }
                "remove" => {
                    self.applied(
                        actions::remove_tool(&self.config, &settings, &app, &tool),
                        format!("Removed {tool} from {app}"),
                    )
                    .await;
                }
                "split" => match actions::split_tool(&self.config, &settings, &app, &tool) {
                    Ok(name) => {
                        reload_daemon().await;
                        self.refresh().await;
                        self.app_links(&name);
                        self.status = format!("{tool} is its own app, {name}");
                    }
                    Err(error) => self.mode = message("Couldn't move it", vec![error.message]),
                },
                _ => {}
            },
            Step::AllowDangerous { app } => {
                self.applied(
                    actions::allow_dangerous(&self.config, &settings, &app),
                    format!("{app} may run code now"),
                )
                .await;
            }
            Step::RevokeClient { id } => {
                self.status = match self.auth.revoke_grant(&id) {
                    Ok(true) => "Access revoked".to_owned(),
                    Ok(false) => "That client was already gone".to_owned(),
                    Err(error) => error.to_string(),
                };
                self.refresh().await;
            }
            Step::RevokeAll => {
                self.status = match self.auth.revoke_all() {
                    Ok(()) => "Revoked every client and token".to_owned(),
                    Err(error) => error.to_string(),
                };
                self.refresh().await;
            }
            Step::TokenLink => {
                self.mode = input(
                    "New token",
                    vec![pair("link", text.clone())],
                    "label",
                    "automation",
                    Step::TokenName { link: text },
                );
            }
            Step::TokenName { link } => {
                let name = if text.is_empty() { "automation".to_owned() } else { text };
                self.mode = input(
                    "New token",
                    vec![pair("link", link.clone()), pair("label", name.clone())],
                    "lasts",
                    "30d",
                    Step::TokenExpires { link, name },
                );
            }
            Step::TokenExpires { link, name } => {
                let Some(ttl) = parse_duration(&text) else {
                    self.mode = Mode::Input {
                        title: "New token".to_owned(),
                        context: vec![pair("link", link.clone()), pair("label", name.clone())],
                        prompt: "lasts".to_owned(),
                        buffer: text,
                        error: Some("Use 30d, 12h or 60m".to_owned()),
                        step: Step::TokenExpires { link, name },
                    };
                    return;
                };

                self.mode = match self.auth.create_static_token(&link, &name, ttl) {
                    Ok((id, token)) => message(
                        "Token created · shown only once",
                        vec![
                            format!("{name} for {link} ({id})"),
                            String::new(),
                            token,
                            String::new(),
                            "Send it as Authorization: Bearer <token>. Revoke it in the Clients tab.".to_owned(),
                        ],
                    ),
                    Err(error) => message("Couldn't create a token", vec![error.to_string()]),
                };
                self.refresh().await;
            }
            Step::OwnTunnel => {
                let address = text.trim_end_matches('/').to_owned();

                if !is_https_url(&address) {
                    self.mode = Mode::Input {
                        title: "Your own https address".to_owned(),
                        context: Vec::new(),
                        prompt: "address".to_owned(),
                        buffer: address,
                        error: Some("It has to start with https://".to_owned()),
                        step: Step::OwnTunnel,
                    };
                    return;
                }

                if self.snap.live.is_none() {
                    if let Err(error) = actions::set_tunnel(&self.config, &address) {
                        self.mode = message("Couldn't save the tunnel", vec![error.message]);
                        return;
                    }
                    self.start_sharing();
                } else {
                    self.use_tunnel(address);
                }
            }
            Step::Stop => {
                self.busy(terminal, "Stopping porchlight…");
                self.mode = match crate::run::stop().await {
                    Ok(()) => {
                        message("Stopped", vec!["Approved clients can reconnect when you share again.".to_owned()])
                    }
                    Err(error) => message("Couldn't stop porchlight", vec![error.message]),
                };
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.service_at = None;
                self.refresh().await;
            }
        }
    }

    async fn find_servers(&mut self, settings: &crate::config::Settings, terminal: &mut DefaultTerminal) {
        self.busy(terminal, "Looking for MCP servers in your clients' settings…");
        let all = discover(settings).await;
        let shared: Vec<String> = settings.mcp.keys().cloned().collect();
        self.found =
            all.into_iter().filter(|server| server.exposable() && server.source != "porchlight.json").collect();

        if self.found.is_empty() {
            self.mode = Mode::Choose {
                title: "Add › MCP servers porchlight found".to_owned(),
                context: Vec::new(),
                question: "Nothing new in your clients' settings.".to_owned(),
                options: vec![
                    choice("Enter an address", "http://127.0.0.1:3000/mcp", "url"),
                    choice("Start one with a command", "npx -y @acme/mcp", "stdio"),
                ],
                cursor: 0,
                step: Step::AddKind,
            };
            return;
        }

        let items = self
            .found
            .iter()
            .map(|server| Check {
                value: server.name.clone(),
                label: server.name.clone(),
                detail: format!(
                    "{:<16}{}",
                    theme::cut(&server.source, 15),
                    server
                        .url
                        .clone()
                        .or_else(|| server.command.as_ref().map(|command| command.join(" ")))
                        .unwrap_or_default()
                ),
                checked: false,
                start: false,
                locked: false,
                gain: "gets its own link".to_owned(),
                lose: String::new(),
            })
            .collect();
        let mut lines =
            vec!["Tick the ones to share. Each gets its own link, and nothing runs until a client asks.".to_owned()];

        if !shared.is_empty() {
            lines.push(format!("already shared: {}", shared.join(", ")));
        }

        self.mode = Mode::Checklist {
            title: "Add › MCP servers porchlight found".to_owned(),
            lines,
            items,
            cursor: 0,
            action: "share".to_owned(),
            note: Some(
                "A server with tools that run code is added with those tools off. You can allow them later.".to_owned(),
            ),
            step: Step::Found,
        };
    }
}

pub async fn run() -> Result<(), ExitError> {
    let internal = |error: &dyn std::fmt::Display| ExitError::new(ExitCode::Internal, error.to_string());
    let config = Config::default_location();
    let store = Store::open_default().map_err(|error| internal(&error))?;
    let snap = snapshot::load(&config, &store).await;
    let first_run = snap.live.is_none() && snap.links.is_empty();
    let mut app = App {
        auth: Auth::new(store.clone()),
        config,
        store,
        snap,
        tab: if first_run { Tab::Links } else { Tab::Apps },
        pane_detail: false,
        list_cursor: 0,
        detail_cursor: 0,
        apps_cursor: 0,
        clients_cursor: 0,
        logs_cursor: 0,
        tunnel_cursor: 0,
        folded: HashSet::new(),
        tool_filter: String::new(),
        apps_filter: String::new(),
        logs: LogView::default(),
        first_run_own: false,
        mode: Mode::Normal,
        status: String::new(),
        quit: false,
        found: Vec::new(),
        job: None,
        searching: None,
        service_at: Some(std::time::Instant::now()),
    };
    let mut terminal = ratatui::try_init().map_err(|error| internal(&error))?;
    let result = event_loop(&mut app, &mut terminal).await;
    let restored = ratatui::try_restore();
    result.and(restored.map_err(|error| internal(&error)))
}

async fn event_loop(app: &mut App, terminal: &mut DefaultTerminal) -> Result<(), ExitError> {
    let mut events = EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(500));
    let mut count: u32 = 0;

    while !app.quit {
        terminal
            .draw(|frame| view::draw(app, frame))
            .map_err(|error| ExitError::new(ExitCode::Internal, error.to_string()))?;

        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => app.key(key, terminal).await,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(ExitError::new(ExitCode::Internal, error.to_string())),
                None => app.quit = true,
            },
            _ = ticks.tick() => {
                count = count.wrapping_add(1);
                app.finish_job(count.is_multiple_of(10)).await;
                if count.is_multiple_of(4) && matches!(app.mode, Mode::Normal | Mode::Code { .. }) {
                    app.refresh().await;
                }
            }
        }
    }

    Ok(())
}
