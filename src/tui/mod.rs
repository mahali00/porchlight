mod act;
mod answer;
mod keys;
mod rows;
mod theme;
mod view;

use crate::auth::Auth;
use crate::config::Config;
use crate::control::reload_daemon;
use crate::discovery::DiscoveredServer;
use crate::exit::{ExitCode, ExitError};
use crate::snapshot::{self, Snapshot};
use crate::spec::CommandSpec;
use crate::store::Store;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Tools,
    Links,
    Clients,
    Logs,
    Tunnel,
}

const TABS: [Tab; 5] = [Tab::Tools, Tab::Links, Tab::Clients, Tab::Logs, Tab::Tunnel];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Self::Links => "Links",
            Self::Tools => "Tools",
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
    CommandDanger { draft: CommandSpec },
    CommandOpenInputs { draft: CommandSpec },
    CommandName { draft: CommandSpec },
    CommandDescription { draft: CommandSpec },
    CommandReadOnly { draft: CommandSpec },
    CommandLinks { draft: CommandSpec },
    EditRun { app: String, tool: String },
    EditAbout { app: String, tool: String, words: Vec<String> },
    EditReads { app: String, tool: String, words: Vec<String>, description: String },
    EditConfirmed { app: String, tool: String, words: Vec<String>, description: String },
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
    Try(Try),
    Code {
        id: String,
        client_id: String,
        client: String,
        link: String,
        copied: bool,
    },
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

fn open_inputs_confirm(command: &str, spec: &CommandSpec, step: Step) -> Mode {
    let caution = crate::explain::caution(spec);
    let mut lines = vec![format!("command   {command}"), String::new()];
    lines.extend(caution.iter().skip(1).cloned());
    lines.extend([String::new(), "esc goes back. To keep it, type add.".to_owned()]);
    typed_confirm(format!("▲ {}", caution.first().cloned().unwrap_or_default()), lines, "add", step)
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
    async fn refresh(&mut self) {
        let fresh = self.service_at.is_some_and(|at| at.elapsed() < Duration::from_secs(30));
        let known = fresh.then(|| self.snap.service.clone());
        self.snap = snapshot::load_with(&self.config, &self.store, known).await;

        if !fresh {
            self.service_at = Some(std::time::Instant::now());
        }

        if let Mode::Code { id, client_id, client, link, .. } = &self.mode
            && !self.snap.pending.iter().any(|pending| pending.id == *id)
        {
            let completed = format!("{client_id} -> {link}");
            let connected = self.snap.clients.iter().any(|known| known.client_id == *client_id && known.link == *link)
                || self.store.audit_list(50).is_ok_and(|entries| {
                    entries.iter().any(|entry| entry.event == "authz.completed" && entry.detail == completed)
                });
            self.mode = if connected {
                Mode::Message {
                    title: "Connected".to_owned(),
                    lines: vec![format!("✓ {client} is connected to {link}.")],
                    danger: false,
                }
            } else {
                message(
                    "Request ended",
                    vec![
                        "The code expired, was entered wrong too many times, or the request was denied.".to_owned(),
                        "Nothing was shared.".to_owned(),
                    ],
                )
            };
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
        tab: if first_run { Tab::Links } else { Tab::Tools },
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
