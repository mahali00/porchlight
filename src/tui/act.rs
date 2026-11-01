use super::{App, Check, Choice, Mode, Step, TABS, Tab, Try, choice, confirm, input, message, plural, typed_confirm};
use crate::actions;
use crate::control::set_daemon_tunnel;
use crate::policy::Level;
use crate::run::ensure_tunnel;
use crate::snapshot::{self};
use crate::system;
use crate::tunnels::Provider;

impl App {
    pub(super) fn start_sharing(&mut self) {
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
                        None => lines.push("Nothing is shared yet. Press + to add a tool or server.".to_owned()),
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

    pub(super) fn switch(&mut self, index: usize) {
        self.tab = TABS.get(index % TABS.len()).copied().unwrap_or(Tab::Tools);
        self.status.clear();
    }

    pub(super) fn show_code(&mut self, id: &str) {
        if let Some(pending) = self.snap.pending.iter().find(|pending| pending.id == id) {
            self.mode = Mode::Code {
                id: pending.id.clone(),
                client_id: pending.client_id.clone(),
                client: pending.client.clone(),
                link: pending.link.clone(),
                copied: false,
            };
        }
    }

    pub(super) async fn deny(&mut self, id: &str) {
        self.status = match actions::deny(&self.store, id) {
            Ok(()) => "Denied the request".to_owned(),
            Err(error) => error.message,
        };
        self.refresh().await;
    }

    pub(super) fn add(&mut self) {
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

    pub(super) fn edit_tool(&mut self, app: String, tool: String) {
        let Some(spec) = crate::spec::lookup(&self.snap.settings, &app, &tool) else {
            self.status = format!("Only command-line tools can be edited here. {app} is an MCP server.");
            return;
        };
        let title = format!("Edit {}", spec.exposed());
        self.mode = input(&title, Vec::new(), "command", &spec.shown(), Step::EditRun { app, tool });
    }

    pub(super) async fn toggle_app(&mut self, app: &str) {
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

    pub(super) fn remove_app(&mut self, app: &str) {
        let shared_on = self.snap.links_of(app);
        let lines = if shared_on.is_empty() {
            vec!["It isn't on any link.".to_owned()]
        } else {
            vec![format!("Clients on {} lose its tools right away.", shared_on.join(", "))]
        };
        self.mode = confirm(format!("Remove {app}?"), lines, Step::RemoveApp { app: app.to_owned() });
    }

    pub(super) fn delete_link(&mut self, link: &str) {
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
            lines.push("The tools stay:".to_owned());
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

    pub(super) fn app_links(&mut self, app: &str) {
        let tools = crate::spec::tool_names(app, self.snap.settings.commands.get(app));
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

    pub(super) fn link_apps(&mut self, link: &str) {
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
                let tools = crate::spec::tool_names(&app.name, self.snap.settings.commands.get(&app.name));
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

    pub(super) fn new_token(&mut self) {
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

    pub(super) fn start_try(&mut self, app: &str, tool: &str) {
        let read_only = self.tool(app, tool).is_some_and(|row| row.level == Level::Reads);

        if self.snap.settings.commands.contains_key(app) || read_only {
            self.try_tool(app, tool);
        } else if self.snap.live.is_none() {
            "Start porchlight to try MCP tools".clone_into(&mut self.status);
        } else {
            self.status = format!("{tool} may change things, so it only runs when a client calls it");
        }
    }

    pub(super) fn try_tool(&mut self, app: &str, tool: &str) {
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
                    level: Level::Changes,
                    resource: false,
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
            changes: row.level.changes(),
            inputs: row.inputs.into_iter().map(|name| (name, String::new())).collect(),
            field: 0,
            output: None,
        });
    }

    pub(super) fn use_tunnel(&mut self, tunnel: String) {
        let config = self.config.clone();
        let running = self.snap.live.is_some();
        self.run_job("Switching the tunnel", move |progress| async move {
            let say = |text: &str| {
                let _ = progress.send(text.to_owned());
            };

            if Provider::named(&tunnel).is_some() {
                say("Installing OpenTunnel 0.5.0 if it's missing");

                if let Err(error) = ensure_tunnel(&tunnel).await {
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
}
