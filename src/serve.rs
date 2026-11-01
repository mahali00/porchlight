use crate::apps::{apps_from, host_environment};
use crate::auth::Auth;
use crate::config::{Config, Settings};
use crate::core::{Problem, now_ms};
use crate::crypto::sha256;
use crate::links::signatures_of;
use crate::registry::Registry;
use crate::store::Store;
use crate::system;
use crate::tunnels::{Opened, Provider, TunnelChoice, TunnelError, tunnel_choice};
use futures::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
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
    let names: IndexSet<&String> = before.links.keys().chain(after.links.keys()).collect();
    let links = names.into_iter().filter_map(|name| match (before.links.get(name), after.links.get(name)) {
        (was, is) if was == is => None,
        (None, _) => Some(format!("link {name} added")),
        (_, None) => Some(format!("link {name} removed")),
        (Some(was), Some(is)) => {
            let added = is.apps().iter().filter(|app| !was.apps().contains(app)).map(|app| format!("+{app}"));
            let removed = was.apps().iter().filter(|app| !is.apps().contains(app)).map(|app| format!("-{app}"));
            let moved: Vec<String> = added.chain(removed).collect();
            Some(if moved.is_empty() {
                format!("link {name} changed")
            } else {
                format!("link {name} {}", moved.join(" "))
            })
        }
    });

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
        let previous = self.applied.lock().ok().and_then(|applied| applied.clone());
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

        let mut revoke_failed = false;

        for server in revoke {
            if let Err(error) = self.auth.revoke_server(server) {
                eprintln!("Couldn't revoke access for {server}: {error}");
                revoke_failed = true;
            }
        }

        if revoke_failed {
            return Err("Couldn't remove access for one or more links".to_owned());
        }
        let signatures = serde_json::to_string(&next).unwrap_or_else(|_| "{}".to_owned());
        self.store.meta_set("link-signatures", &signatures).map_err(|error| failed(&error))?;
        self.registry.set_rules(apps.rules).await;
        self.registry.sync(apps.specs, apps.links).await;
        let changes =
            previous.as_ref().map(|previous| describe_changes(previous, &loaded.settings)).unwrap_or_default();

        if !changes.is_empty() {
            let _ = self.store.audit_write("config.changed", &changes.join("; "));
        }
        if let Ok(mut applied) = self.applied.lock() {
            *applied = Some(loaded.settings.clone());
        }

        self.set_problems(loaded.problems.into_iter().chain(apps.problems).collect());

        Ok(())
    }

    async fn reload_with(&self, current: String) {
        let _locked = self.lock.lock().await;

        if let Err(message) = self.apply().await {
            self.set_problems(vec![Problem {
                app: String::new(),
                message: format!("{message}. Still using the last good settings."),
            }]);
            return;
        }

        if let Ok(mut seen) = self.seen.lock() {
            *seen = current;
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

#[cfg(test)]
mod tests {
    use super::Reloader;
    use crate::auth::Auth;
    use crate::config::Config;
    use crate::registry::Registry;
    use crate::store::Store;
    use std::sync::Mutex;

    #[tokio::test]
    async fn a_failed_apply_does_not_mark_the_config_fingerprint_as_seen() {
        let path = std::env::temp_dir().join(format!("porchlight-reload-{}.json", crate::crypto::random_token(6)));
        std::fs::write(&path, "{").unwrap();
        let store = Store::in_memory().unwrap();
        let reloader = Reloader {
            registry: Registry::new(store.clone(), "test-host".to_owned()),
            config: Config::at(path.clone()),
            store: store.clone(),
            auth: Auth::new(store),
            applied: Mutex::new(None),
            problems: Mutex::new(Vec::new()),
            seen: Mutex::new(String::new()),
            lock: tokio::sync::Mutex::new(()),
        };

        reloader.reload_with("bad-config-fingerprint".to_owned()).await;

        assert_eq!(reloader.seen.lock().unwrap().as_str(), "");
        std::fs::remove_file(path).unwrap();
    }
}
