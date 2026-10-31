use super::{
    Answer, App, ClientRow, DetailRow, ListRow, Mode, Search, Step, TABS, Tab, Try, TunnelRow, choice, confirm, input,
    message, plural, typed_confirm, view,
};
use crate::actions;
use crate::apps::host_environment;
use crate::system;
use crate::tunnels::DEFAULT_TUNNEL;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::DefaultTerminal;
use serde_json::json;

impl App {
    pub(super) async fn key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) {
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
            Mode::Message { .. } | Mode::Help => {}
            Mode::Code { id, client_id, client, link, copied } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Char('y') => {
                    let code =
                        self.snap.pending.iter().find(|pending| pending.id == id).map(|pending| pending.code.clone());
                    let copied = match code {
                        Some(code) => system::copy_to_clipboard(&code).await,
                        None => copied,
                    };
                    self.mode = Mode::Code { id, client_id, client, link, copied };
                }
                KeyCode::Char('x') => self.deny(&id).await,
                _ => self.mode = Mode::Code { id, client_id, client, link, copied },
            },
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

    pub(super) fn search_key(&mut self, target: Search, key: KeyEvent) {
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

    pub(super) async fn normal_key(&mut self, key: KeyEvent) {
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
            Tab::Tools => self.apps_key(key).await,
            Tab::Clients => self.clients_key(key).await,
            Tab::Logs => self.logs_key(key),
            Tab::Tunnel => self.tunnel_key(key).await,
        }
    }

    pub(super) fn first_run_key(&mut self, key: KeyEvent) {
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

    pub(super) async fn links_key(&mut self, key: KeyEvent) {
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

    pub(super) async fn list_key(&mut self, key: KeyEvent) {
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

    pub(super) async fn detail_key(&mut self, key: KeyEvent) {
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
                        choice("Move it to its own link", "then add the link to other clients", "split"),
                    ],
                    cursor: 0,
                    step: Step::RemoveTool { app, tool },
                };
            }
            _ => {}
        }
    }

    pub(super) async fn apps_key(&mut self, key: KeyEvent) {
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
                let tools = crate::spec::tool_names(&app.name, self.snap.settings.commands.get(&app.name));
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

    pub(super) async fn clients_key(&mut self, key: KeyEvent) {
        let Some(row) = self.client_rows().get(self.clients_cursor).cloned() else { return };
        match (row, key.code) {
            (ClientRow::Pending(id), KeyCode::Enter | KeyCode::Char('a')) => self.show_code(&id),
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

    pub(super) fn logs_key(&mut self, key: KeyEvent) {
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

    pub(super) async fn tunnel_key(&mut self, key: KeyEvent) {
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

    pub(super) async fn try_key(&mut self, mut state: Try, key: KeyEvent, terminal: &mut DefaultTerminal) {
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
                self.mode = Mode::Try(state);
                let _ = terminal.draw(|frame| view::draw(self, frame));
                let Mode::Try(mut state) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
                let started = std::time::Instant::now();
                let (ok, output) = if self.snap.settings.commands.contains_key(&state.app) {
                    let values =
                        actions::values_from_pairs(&self.snap.settings, &state.app, &state.tool, &state.inputs);
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
}
