use super::{App, Check, ClientRow, DetailRow, ListRow, Tab, TunnelRow};
use crate::snapshot::{self, Verdict};

impl App {
    pub(super) fn list_rows(&self) -> Vec<ListRow> {
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

    pub(super) fn selected_link(&self) -> Option<&snapshot::LinkInfo> {
        match self.list_rows().get(self.list_cursor) {
            Some(ListRow::Link(name)) => self.snap.links.iter().find(|link| link.name == *name),
            _ => None,
        }
    }

    pub(super) fn app_rows(&self) -> Vec<&snapshot::AppInfo> {
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

    pub(super) fn matched_tools<'a>(&self, info: &'a snapshot::AppInfo) -> Vec<&'a str> {
        let filter = self.apps_filter.to_lowercase();

        if filter.is_empty() {
            return Vec::new();
        }

        info.tool_names.iter().filter(|name| name.to_lowercase().contains(&filter)).map(String::as_str).collect()
    }

    pub(super) fn detail_rows(&self) -> Vec<DetailRow> {
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

    pub(super) fn tool(&self, app: &str, tool: &str) -> Option<&snapshot::ToolRow> {
        self.selected_link()?.groups.iter().find(|group| group.name == app)?.tools.iter().find(|row| row.tool == tool)
    }

    pub(super) fn client_rows(&self) -> Vec<ClientRow> {
        self.snap
            .pending
            .iter()
            .map(|pending| ClientRow::Pending(pending.id.clone()))
            .chain(self.snap.clients.iter().map(|client| ClientRow::Client(client.id.clone())))
            .chain(std::iter::once(ClientRow::NewToken))
            .collect()
    }

    pub(super) fn log_rows(&self) -> Vec<&snapshot::LogRow> {
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

    pub(super) fn tunnel_rows() -> [TunnelRow; 2] {
        [TunnelRow::Open, TunnelRow::Own]
    }

    pub(super) fn count(&self) -> usize {
        match self.tab {
            Tab::Links if self.pane_detail => self.detail_rows().len(),
            Tab::Links => self.list_rows().len(),
            Tab::Tools => self.app_rows().len(),
            Tab::Clients => self.client_rows().len(),
            Tab::Logs => self.log_rows().len(),
            Tab::Tunnel => 2,
        }
    }

    pub(super) fn cursor_mut(&mut self) -> &mut usize {
        match self.tab {
            Tab::Links if self.pane_detail => &mut self.detail_cursor,
            Tab::Links => &mut self.list_cursor,
            Tab::Tools => &mut self.apps_cursor,
            Tab::Clients => &mut self.clients_cursor,
            Tab::Logs => &mut self.logs_cursor,
            Tab::Tunnel => &mut self.tunnel_cursor,
        }
    }

    pub(super) fn clamp(&mut self) {
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

    pub(super) fn pending_here(&self) -> Option<String> {
        match self.client_rows().get(self.clients_cursor) {
            Some(ClientRow::Pending(id)) => Some(id.clone()),
            _ => None,
        }
    }

    pub(super) fn clients_of(&self, link: &str) -> Vec<String> {
        self.snap.clients.iter().filter(|client| client.link == link).map(|client| client.name.clone()).collect()
    }

    pub(super) fn link_checks(&self, app: &str) -> Vec<Check> {
        let current = self.snap.links_of(app);
        let tools = crate::spec::tool_names(app, self.snap.settings.commands.get(app));

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
}
