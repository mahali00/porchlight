use crate::apps::AppSpec;
use crate::core::Tool;
use crate::links::{LinkEntry, LinkPlan, PhysicalApp, Projection, ServerState};
use crate::policy::{Rules, allowed_tool_names, app_default_on};
use crate::sources::{ListChange, Notices};
use crate::store::Store;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use indexmap::{IndexMap, IndexSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::WatchStream;

const RETRY_EVERY: Duration = Duration::from_secs(3);

const HTTP_REFRESH_EVERY: Duration = Duration::from_secs(5);

const SETTLE_ATTEMPTS: usize = 120;

const SETTLE_EVERY: Duration = Duration::from_millis(50);

struct Slot {
    entry: watch::Sender<PhysicalApp>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl Slot {
    fn current(&self) -> PhysicalApp {
        self.entry.borrow().clone()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Inventory {
    tools: String,
    resources: String,
}

fn inventory(entry: &PhysicalApp) -> Inventory {
    let tools: Vec<&Tool> = entry.tools.iter().filter(|tool| entry.allowed.contains(&tool.name)).collect();

    Inventory {
        tools: serde_json::to_string(&tools).unwrap_or_default(),
        resources: serde_json::to_string(&entry.spec.source.resources(&entry.allowed)).unwrap_or_default(),
    }
}

fn projected_inventory(entry: Option<&LinkEntry>) -> Inventory {
    let tools: Vec<&Tool> = entry
        .map(|entry| entry.tools.iter().filter(|tool| entry.allowed.contains(&tool.name)).collect())
        .unwrap_or_default();
    let resources: Vec<&String> = entry
        .map(|entry| {
            entry.resources.iter().filter(|(_, tool)| entry.allowed.contains(*tool)).map(|(uri, _)| uri).collect()
        })
        .unwrap_or_default();

    Inventory {
        tools: serde_json::to_string(&tools).unwrap_or_default(),
        resources: serde_json::to_string(&resources).unwrap_or_default(),
    }
}

fn changed_lists(last: &Inventory, next: &Inventory) -> Vec<ListChange> {
    let mut changes = Vec::new();

    if last.tools != next.tools {
        changes.push(ListChange::Tools);
    }

    if last.resources != next.resources {
        changes.push(ListChange::Resources);
    }

    changes
}

fn notices_of(inventories: BoxStream<'static, Inventory>, initial: Inventory) -> BoxStream<'static, ListChange> {
    inventories
        .scan(initial, |last, next| {
            let changes = changed_lists(last, &next);
            *last = next;
            futures::future::ready(Some(stream::iter(changes)))
        })
        .flatten()
        .boxed()
}

fn waiting(entry: &PhysicalApp) -> PhysicalApp {
    PhysicalApp {
        state: ServerState::Waiting,
        detail: format!("waiting for {} to start", entry.spec.name),
        ..entry.clone()
    }
}

fn checked(entry: &PhysicalApp, tools: Vec<Tool>, rules: &Rules) -> PhysicalApp {
    let allowed = allowed_tool_names(&tools, rules, &entry.spec.name);
    let default_on = app_default_on(rules, &entry.spec.name);
    let dangerous: Vec<&str> = tools
        .iter()
        .filter(|tool| allowed.contains(&tool.name) && entry.spec.source.risky(tool))
        .map(|tool| tool.name.as_str())
        .collect();

    if dangerous.is_empty() || entry.spec.allow_dangerous {
        let detail = format!("{} of {} tools", allowed.len(), tools.len());
        return PhysicalApp { tools, allowed, default_on, state: ServerState::Live, detail, ..entry.clone() };
    }

    let named: Vec<&str> = dangerous.into_iter().take(5).collect();
    let detail = format!(
        "refused: {} can run commands or code. Turn them off with \"{}/<tool>\": false in tools, or set allowDangerous if you trust every client",
        named.join(", "),
        entry.spec.name
    );

    PhysicalApp { tools, allowed, default_on, state: ServerState::Refused, detail, ..entry.clone() }
}

struct Inner {
    store: Store,
    host_name: String,
    slots: Mutex<IndexMap<String, Arc<Slot>>>,
    rules: Mutex<Rules>,
    rules_lock: tokio::sync::Mutex<()>,
    plans: RwLock<Vec<LinkPlan>>,
    revision: watch::Sender<u64>,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

impl Inner {
    fn slot(&self, name: &str) -> Option<Arc<Slot>> {
        self.slots.lock().ok().and_then(|slots| slots.get(name).cloned())
    }

    fn get(&self, name: &str) -> Option<PhysicalApp> {
        self.slot(name).map(|slot| slot.current())
    }

    fn announce(&self) {
        self.revision.send_modify(|revision| *revision += 1);
    }

    fn project(self: &Arc<Self>, plan: &LinkPlan, notices: Notices) -> Option<LinkEntry> {
        let find = |name: &str| self.get(name);

        plan.project(&Projection { app: &find, notices, host_name: self.host_name.clone() })
    }

    fn app_notices(self: &Arc<Self>, name: &str) -> BoxStream<'static, ListChange> {
        let Some(slot) = self.slot(name) else {
            return stream::empty().boxed();
        };
        let initial = inventory(&slot.current());
        let changes = WatchStream::from_changes(slot.entry.subscribe()).map(|entry| inventory(&entry)).boxed();

        notices_of(changes, initial)
    }

    fn link_notices(self: &Arc<Self>, plan: &LinkPlan) -> BoxStream<'static, ListChange> {
        let quiet: Notices = Arc::new(|| stream::empty().boxed());
        let current = {
            let inner = Arc::downgrade(self);
            let plan = plan.clone();
            move || {
                let entry = inner.upgrade().and_then(|inner| inner.project(&plan, quiet.clone()));
                projected_inventory(entry.as_ref())
            }
        };
        let initial = current();
        let changes = WatchStream::from_changes(self.revision.subscribe()).map(move |_| current()).boxed();

        notices_of(changes, initial)
    }

    fn notices_for_plan(self: &Arc<Self>, plan: &LinkPlan) -> Notices {
        let inner = Arc::downgrade(self);
        let plan = plan.clone();

        Arc::new(move || match inner.upgrade() {
            Some(inner) => inner.link_notices(&plan),
            None => stream::empty().boxed(),
        })
    }

    async fn check(self: &Arc<Self>, slot: &Slot) -> bool {
        let source = slot.current().source;
        let tools = source.list_tools().await.ok();
        let _locked = self.rules_lock.lock().await;
        let rules = self.rules.lock().map(|rules| rules.clone()).unwrap_or_default();
        slot.entry.send_modify(|entry| {
            *entry = match tools {
                None => waiting(entry),
                Some(tools) => checked(entry, tools, &rules),
            };
        });
        let settled = slot.current().state != ServerState::Waiting;
        self.announce();

        settled
    }

    fn supervise(self: &Arc<Self>, name: &str) {
        let Some(slot) = self.slot(name) else { return };
        let refresh = slot.current().spec.source.refresh();
        let inner = Arc::downgrade(self);
        let watched = Arc::downgrade(&slot);
        let task = tokio::spawn(async move {
            loop {
                let (Some(inner), Some(slot)) = (inner.upgrade(), watched.upgrade()) else {
                    return;
                };
                let settled = inner.check(&slot).await;
                drop(slot);
                drop(inner);

                if settled && !refresh {
                    return;
                }

                tokio::time::sleep(if refresh && settled { HTTP_REFRESH_EVERY } else { RETRY_EVERY }).await;
            }
        });

        if let Ok(mut supervisor) = slot.supervisor.lock()
            && let Some(previous) = supervisor.replace(task)
        {
            previous.abort();
        }
    }
}

impl Registry {
    pub fn new(store: Store, host_name: String) -> Self {
        let (revision, _) = watch::channel(0);

        Self {
            inner: Arc::new(Inner {
                store,
                host_name,
                slots: Mutex::new(IndexMap::new()),
                rules: Mutex::new(Rules::new()),
                rules_lock: tokio::sync::Mutex::new(()),
                plans: RwLock::new(Vec::new()),
                revision,
            }),
        }
    }

    pub fn get(&self, name: &str) -> Option<PhysicalApp> {
        self.inner.get(name)
    }

    pub fn changes(&self, name: &str) -> Option<watch::Receiver<PhysicalApp>> {
        self.inner.slot(name).map(|slot| slot.entry.subscribe())
    }

    pub fn list(&self) -> Vec<PhysicalApp> {
        self.inner.slots.lock().map(|slots| slots.values().map(|slot| slot.current()).collect()).unwrap_or_default()
    }

    pub async fn remove(&self, name: &str) -> bool {
        let slot = self.inner.slots.lock().ok().and_then(|mut slots| slots.shift_remove(name));
        let Some(slot) = slot else { return false };

        if let Some(supervisor) = slot.supervisor.lock().ok().and_then(|mut supervisor| supervisor.take()) {
            supervisor.abort();
        }

        slot.current().source.shutdown().await;
        self.inner.announce();

        true
    }

    pub async fn add(&self, spec: AppSpec) -> PhysicalApp {
        self.remove(&spec.name).await;
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let name = spec.name.clone();
        let notices: Notices = Arc::new(move || match weak.upgrade() {
            Some(inner) => inner.app_notices(&name),
            None => stream::empty().boxed(),
        });
        let source = spec.source.connect(notices, &self.inner.store);
        let entry = PhysicalApp {
            spec: spec.clone(),
            source,
            state: ServerState::Starting,
            detail: "starting".to_owned(),
            tools: Vec::new(),
            allowed: IndexSet::new(),
            default_on: true,
        };
        let (sender, _) = watch::channel(entry.clone());
        let slot = Arc::new(Slot { entry: sender, supervisor: Mutex::new(None) });

        if let Ok(mut slots) = self.inner.slots.lock() {
            slots.insert(spec.name.clone(), slot);
        }

        self.inner.supervise(&spec.name);

        entry
    }

    pub fn mark_down(&self, name: &str) {
        let Some(slot) = self.inner.slot(name) else {
            return;
        };
        let was_live = slot.entry.send_if_modified(|entry| {
            if entry.state != ServerState::Live {
                return false;
            }

            *entry = waiting(entry);
            true
        });

        if was_live {
            self.inner.announce();
            self.inner.supervise(name);
        }
    }

    pub async fn settled(&self, name: &str) -> Option<PhysicalApp> {
        for _ in 0..SETTLE_ATTEMPTS {
            match self.get(name) {
                Some(current) if current.state == ServerState::Starting => tokio::time::sleep(SETTLE_EVERY).await,
                other => return other,
            }
        }

        self.get(name)
    }

    pub async fn settle_all(&self) {
        let names: Vec<String> = self.list().into_iter().map(|entry| entry.spec.name).collect();
        futures::future::join_all(names.iter().map(|name| self.settled(name))).await;
    }

    pub async fn set_rules(&self, rules: Rules) {
        let _locked = self.inner.rules_lock.lock().await;

        if let Ok(mut current) = self.inner.rules.lock() {
            current.clone_from(&rules);
        }

        let slots: Vec<Arc<Slot>> =
            self.inner.slots.lock().map(|slots| slots.values().cloned().collect()).unwrap_or_default();

        for slot in slots {
            slot.entry.send_modify(|entry| {
                if matches!(entry.state, ServerState::Live | ServerState::Refused) {
                    *entry = checked(entry, entry.tools.clone(), &rules);
                }
            });
        }

        self.inner.announce();
    }

    fn plans(&self) -> Vec<LinkPlan> {
        self.inner.plans.read().map(|plans| plans.clone()).unwrap_or_default()
    }

    pub fn link(&self, name: &str) -> Option<LinkEntry> {
        let plan = self.plans().into_iter().find(|plan| plan.name() == name)?;

        self.inner.project(&plan, self.inner.notices_for_plan(&plan))
    }

    pub fn links(&self) -> Vec<LinkEntry> {
        self.plans().iter().filter_map(|plan| self.inner.project(plan, self.inner.notices_for_plan(plan))).collect()
    }

    pub async fn sync(&self, specs: Vec<AppSpec>, plans: Vec<LinkPlan>) {
        let wanted: IndexSet<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        let current: Vec<String> =
            self.inner.slots.lock().map(|slots| slots.keys().cloned().collect()).unwrap_or_default();

        for name in current.iter().filter(|name| !wanted.contains(name.as_str())) {
            self.remove(name).await;
        }

        for spec in specs {
            let same = self.get(&spec.name).is_some_and(|current| current.spec == spec);

            if !same {
                self.add(spec).await;
            }
        }

        if let Ok(mut current) = self.inner.plans.write() {
            *current = plans;
        }

        self.inner.announce();
    }

    pub async fn shutdown(&self) {
        let names: Vec<String> =
            self.inner.slots.lock().map(|slots| slots.keys().cloned().collect()).unwrap_or_default();

        for name in names {
            self.remove(&name).await;
        }
    }
}
