use crate::apps::{apps_from, host_environment};
use crate::auth::Auth;
use crate::config::{Config, Settings};
use crate::core::{Problem, link_path, now_ms};
use crate::crypto::sha256;
use crate::links::{LinkEntry, ServerState, signatures_of};
use crate::registry::Registry;
use crate::store::Store;
use crate::system;
use crate::tunnels::{Opened, Provider, TunnelChoice, TunnelError, tunnel_choice};
use futures::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[derive(Clone)]
pub struct Shared {
    pub store: Store,
    pub auth: Auth,
    pub registry: Registry,
    pub client_metadata: crate::client_metadata::ClientMetadata,
    pub config: Config,
}

pub struct Daemon {
    pub gateway_port: u16,
    pub approval_port: u16,
    active: tokio::sync::Mutex<Option<(Provider, Opened)>>,
    public_url: RwLock<String>,
}

impl Daemon {
    pub async fn start(gateway_port: u16, approval_port: u16, tunnel: &str) -> Result<Arc<Self>, TunnelError> {
        let daemon = Arc::new(Self {
            gateway_port,
            approval_port,
            active: tokio::sync::Mutex::new(None),
            public_url: RwLock::new(String::new()),
        });
        daemon.connect(tunnel).await?;

        Ok(daemon)
    }

    fn set_url(&self, url: &str) {
        if let Ok(mut current) = self.public_url.write() {
            url.clone_into(&mut current);
        }
    }

    async fn connect(&self, choice: &str) -> Result<String, TunnelError> {
        let mut active = self.active.lock().await;

        if let Some((_, opened)) = active.take() {
            opened.close().await;
        }

        match tunnel_choice(choice) {
            Some(TunnelChoice::Url(url)) => {
                self.set_url(&url);
                Ok(url)
            }
            Some(TunnelChoice::Provider(provider)) => {
                let opened = provider.open(self.gateway_port).await?;
                let url = opened.public_url().to_owned();
                self.set_url(&url);
                *active = Some((provider, opened));
                Ok(url)
            }
            None => {
                let names: Vec<&str> = Provider::ALL.iter().map(|provider| provider.name()).collect();
                Err(TunnelError::new(format!(
                    "Unknown tunnel \"{choice}\". Use {} or an https:// URL.",
                    names.join(", ")
                )))
            }
        }
    }

    pub fn public_url(&self) -> String {
        self.public_url.read().map(|url| url.clone()).unwrap_or_default()
    }

    pub async fn provider(&self) -> Option<Provider> {
        self.active.lock().await.as_ref().map(|(provider, _)| *provider)
    }

    pub async fn set_tunnel(&self, choice: &str) -> Result<String, TunnelError> {
        let current = self.public_url();
        let unchanged = match self.provider().await {
            Some(provider) => provider.name() == choice,
            None => choice == current,
        };

        if unchanged { Ok(current) } else { self.connect(choice).await }
    }

    pub async fn close(&self) {
        if let Some((_, opened)) = self.active.lock().await.take() {
            opened.close().await;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerView {
    pub name: String,
    pub url: String,
    pub state: ServerState,
    pub detail: String,
    pub tools: usize,
    pub names: Vec<String>,
    #[serde(default)]
    pub catalog: Vec<ToolInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub app: String,
    pub tool: String,
    pub about: Option<String>,
    pub read_only: Option<bool>,
    pub resource: bool,
    pub dangerous: bool,
    pub on: bool,
    #[serde(default)]
    pub inputs: Vec<String>,
}

pub fn view_of(entry: &LinkEntry, public_url: &str) -> ServerView {
    let mut names: Vec<String> = entry.allowed.iter().cloned().collect();
    names.sort();
    let catalog = entry
        .tools
        .iter()
        .map(|tool| {
            let origin = entry.origins.get(&tool.name);
            ToolInfo {
                name: tool.name.clone(),
                app: origin.map_or_else(|| entry.name.clone(), |origin| origin.app.clone()),
                tool: origin.map_or_else(|| tool.name.clone(), |origin| origin.tool.clone()),
                about: tool.description.clone(),
                read_only: tool.annotations.as_ref().and_then(|annotations| annotations.read_only_hint),
                resource: entry.resources.values().any(|exposed| *exposed == tool.name),
                dangerous: entry.source_app.is_some() && crate::policy::is_dangerous(tool),
                on: entry.allowed.contains(&tool.name),
                inputs: tool
                    .input_schema
                    .as_ref()
                    .and_then(|schema| schema.get("properties"))
                    .and_then(serde_json::Value::as_object)
                    .map(|properties| properties.keys().cloned().collect())
                    .unwrap_or_default(),
            }
        })
        .collect();

    ServerView {
        name: entry.name.clone(),
        url: format!("{public_url}{}", link_path(&entry.name)),
        state: entry.state,
        detail: entry.detail.clone(),
        tools: entry.allowed.len(),
        names,
        catalog,
    }
}

fn apps_of(settings: &Settings) -> IndexMap<String, String> {
    settings
        .mcp
        .iter()
        .map(|(name, app)| (name.clone(), serde_json::to_string(app).unwrap_or_default()))
        .chain(
            settings.commands.iter().map(|(name, app)| (name.clone(), serde_json::to_string(app).unwrap_or_default())),
        )
        .collect()
}

pub fn describe_changes(before: &Settings, after: &Settings) -> Vec<String> {
    let old_apps = apps_of(before);
    let new_apps = apps_of(after);
    let names: IndexSet<&String> = old_apps.keys().chain(new_apps.keys()).collect();
    let apps = names.into_iter().filter_map(|name| match (old_apps.get(name), new_apps.get(name)) {
        (was, is) if was == is => None,
        (None, _) => Some(format!("{name} added")),
        (_, None) => Some(format!("{name} removed")),
        _ => Some(format!("{name} changed")),
    });
    let patterns: IndexSet<&String> = before.tools.keys().chain(after.tools.keys()).collect();
    let rules =
        patterns.into_iter().filter_map(|pattern| match (before.tools.get(pattern), after.tools.get(pattern)) {
            (was, is) if was == is => None,
            (_, None) => Some(format!("{pattern} rule removed")),
            (_, Some(true)) => Some(format!("{pattern} on")),
            (_, Some(false)) => Some(format!("{pattern} off")),
        });
    let links = (before.links != after.links).then(|| "links changed".to_owned());

    apps.chain(rules).chain(links).collect()
}

fn fingerprint(config: &Config) -> String {
    std::fs::read_to_string(&config.path).map(|text| sha256(&text)).unwrap_or_default()
}

pub trait Reload: Send + Sync {
    fn reload(&self) -> BoxFuture<'_, ()>;
    fn problems(&self) -> Vec<Problem>;
}

pub struct Reloader {
    registry: Registry,
    config: Config,
    store: Store,
    auth: Auth,
    applied: Mutex<Option<Settings>>,
    problems: Mutex<Vec<Problem>>,
    seen: Mutex<String>,
    lock: tokio::sync::Mutex<()>,
}

impl Reloader {
    pub async fn start(registry: Registry, config: Config, store: Store, auth: Auth) -> Arc<Self> {
        let reloader = Arc::new(Self {
            registry,
            config,
            store,
            auth,
            applied: Mutex::new(None),
            problems: Mutex::new(Vec::new()),
            seen: Mutex::new(String::new()),
            lock: tokio::sync::Mutex::new(()),
        });
        reloader.reload_now().await;
        let watcher = Arc::downgrade(&reloader);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(reloader) = watcher.upgrade() else {
                    return;
                };
                reloader.reload_if_changed().await;
            }
        });

        reloader
    }

    fn set_problems(&self, problems: Vec<Problem>) {
        if let Ok(mut current) = self.problems.lock() {
            *current = problems;
        }
    }

    async fn apply(&self) -> Result<(), String> {
        let loaded = self.config.load().map_err(|error| error.message)?;
        let apps = apps_from(&loaded.settings, &host_environment(), Some(&loaded.commands));
        let previous = self.applied.lock().ok().and_then(|mut applied| applied.replace(loaded.settings.clone()));
        let failed = |error: &dyn std::fmt::Display| error.to_string();
        let granted: IndexSet<String> =
            self.store.grants_list().map_err(|error| failed(&error))?.into_iter().map(|grant| grant.server).collect();
        let waiting: IndexSet<String> = self
            .store
            .pending_list()
            .map_err(|error| failed(&error))?
            .into_iter()
            .map(|request| request.server)
            .collect();
        let stored = self.store.meta_get("link-signatures").map_err(|error| failed(&error))?;
        let old: IndexMap<String, String> =
            stored.and_then(|value| serde_json::from_str(&value).ok()).unwrap_or_default();
        let compiled = signatures_of(&apps.links);
        let next: IndexMap<&String, &String> = loaded
            .links
            .iter()
            .filter_map(|name| compiled.get(name).or_else(|| old.get(name)).map(|signature| (name, signature)))
            .collect();
        let removed_links =
            granted.iter().chain(waiting.iter()).chain(old.keys()).filter(|server| !loaded.links.contains(*server));
        let new_membership =
            compiled.iter().filter(|(server, signature)| old.get(*server) != Some(signature)).map(|(server, _)| server);
        let revoke: IndexSet<&String> = removed_links.chain(new_membership).collect();

        for server in revoke {
            self.auth.revoke_server(server).map_err(|error| failed(&error))?;
        }

        let signatures = serde_json::to_string(&next).unwrap_or_else(|_| "{}".to_owned());
        self.store.meta_set("link-signatures", &signatures).map_err(|error| failed(&error))?;
        self.registry.set_rules(apps.rules).await;
        self.registry.sync(apps.specs, apps.links).await;
        let changes = previous.map(|previous| describe_changes(&previous, &loaded.settings)).unwrap_or_default();

        if !changes.is_empty() {
            let _ = self.store.audit_write("config.changed", &changes.join("; "));
        }

        self.set_problems(loaded.problems.into_iter().chain(apps.problems).collect());

        Ok(())
    }

    async fn reload_with(&self, current: String) {
        let _locked = self.lock.lock().await;

        if let Ok(mut seen) = self.seen.lock() {
            *seen = current;
        }

        if let Err(message) = self.apply().await {
            self.set_problems(vec![Problem {
                app: String::new(),
                message: format!("{message}. Still using the last good settings."),
            }]);
        }
    }

    pub async fn reload_now(&self) {
        self.reload_with(fingerprint(&self.config)).await;
    }

    async fn reload_if_changed(&self) {
        let current = fingerprint(&self.config);
        let seen = self.seen.lock().map(|seen| seen.clone()).unwrap_or_default();

        if current != seen {
            self.reload_with(current).await;
        }
    }
}

impl Reload for Reloader {
    fn reload(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.reload_now())
    }

    fn problems(&self) -> Vec<Problem> {
        self.problems.lock().map(|problems| problems.clone()).unwrap_or_default()
    }
}

const STALE_CLIENT_AGE: i64 = 24 * 60 * 60 * 1000;

const AUDIT_RETENTION: i64 = 90 * 24 * 60 * 60 * 1000;

fn sweep(store: &Store) {
    let now = now_ms();
    let _ = store.pending_prune(now);
    let _ = store.clients_prune(now - STALE_CLIENT_AGE);
    let _ = store.tokens_prune(now);
    let _ = store.grants_prune(now - 60 * 60 * 1000);
    let _ = store.audit_prune(now - AUDIT_RETENTION);
}

pub fn upkeep(store: Store, daemon: Arc<Daemon>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut warned: IndexSet<String> = IndexSet::new();
        let mut sweeps = tokio::time::interval(Duration::from_mins(5));
        let mut warnings = tokio::time::interval(Duration::from_hours(6));

        loop {
            tokio::select! {
                _ = sweeps.tick() => sweep(&store),
                _ = warnings.tick() => {
                    let Some(provider) = daemon.provider().await else { continue };

                    for warning in provider.warnings().await {
                        if warned.insert(warning.id) {
                            system::notify("porchlight", &warning.message).await;
                        }
                    }
                }
            }
        }
    })
}
