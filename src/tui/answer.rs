use super::{
    Answer, App, Check, Mode, Step, choice, input, message, open_inputs_confirm, pair, plural, theme, typed_confirm,
};
use crate::actions;
use crate::control::reload_daemon;
use crate::core::{is_app_name, is_http_url, is_https_url};
use crate::crypto::parse_duration;
use crate::discovery::{DiscoveredServer, discover, is_local_url};
use crate::spec::CommandSpec;
use crate::system;
use ratatui::DefaultTerminal;
use serde_json::json;
use std::time::Duration;

impl App {
    pub(super) fn links_checklist(&self, draft: CommandSpec, context: &[(String, String)]) -> Mode {
        let items = self.link_checks(&draft.app);
        Mode::Checklist {
            title: format!("Add an app › {}", draft.app),
            lines: context
                .iter()
                .map(|(label, value)| format!("{label:<16}{value}"))
                .chain([String::new(), format!("Which links should have {}?", draft.app)])
                .collect(),
            items,
            cursor: 0,
            action: "add".to_owned(),
            note: Some(
                "MCP servers have their own links, so they aren't listed. Tick none to keep it under \"not shared\"."
                    .to_owned(),
            ),
            step: Step::CommandLinks { draft },
        }
    }

    pub(super) fn draft_context(draft: &CommandSpec) -> Vec<(String, String)> {
        let mut context = vec![pair("kind", "command-line tool"), pair("command", draft.shown())];

        if !draft.app.is_empty() {
            context.push(pair("name", draft.app.clone()));
        }

        if let Some(description) = &draft.description {
            context.push(pair("what it does", description.clone()));
        }

        context
    }

    pub(super) async fn answer(&mut self, step: Step, answer: Answer, terminal: &mut DefaultTerminal) {
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
                    let name = actions::import_name(&found, &latest);
                    self.busy(terminal, &format!("Sharing {name}…"));
                    match actions::share_server(&self.config, &name, &serde_json::to_value(&entry).unwrap_or_default())
                        .await
                    {
                        Ok(shared) => lines.extend(crate::explain::shared(&shared)),
                        Err(error) => lines.push(error.message),
                    }
                }

                self.mode = message("Share results", lines);
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
                        crate::core::slugify(&format!(
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
                self.mode = match actions::share_server(&self.config, &text, &json!({ "url": url })).await {
                    Ok(shared) => message("Shared", crate::explain::shared(&shared)),
                    Err(error) => message("Couldn't share server", vec![error.message]),
                };
                self.refresh().await;
            }
            Step::StdioCommand => {
                let command = crate::spec::command_words(&text);

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
                self.mode = match actions::share_server(
                    &self.config,
                    &text,
                    &json!({ "command": crate::spec::run_value(&command) }),
                )
                .await
                {
                    Ok(shared) => message("Shared", crate::explain::shared(&shared)),
                    Err(error) => message("Couldn't share server", vec![error.message]),
                };
                self.refresh().await;
            }
            Step::CommandRun => {
                let draft = CommandSpec::draft("", crate::spec::command_words(&text));

                if let Some(problem) = actions::program_problem(draft.program()) {
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

                self.mode = if draft.risk().runs_code {
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

                draft.app.clone_from(&text);
                draft.tool = text;
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
                let draft = draft.honest();
                let risk = draft.risk();
                let mut context = Self::draft_context(&draft);
                context.push(pair(
                    "changes things",
                    if risk.reads() { "no, it only reads".to_owned() } else { risk.summary() },
                ));

                if risk.needs_person() {
                    self.mode =
                        open_inputs_confirm(&draft.shown(), &draft, Step::CommandOpenInputs { draft: draft.clone() });
                    return;
                }

                if draft.read_only && draft.placeholders().is_empty() {
                    self.busy(terminal, &format!("Running {} once…", draft.shown()));
                    let tried = actions::try_draft(&draft).await;
                    let first = tried.text.lines().next().unwrap_or_default().chars().take(70).collect::<String>();
                    context.push(pair("ran it once", format!("{} {first}", if tried.ok { "✓" } else { "✗" })));
                }

                self.mode = self.links_checklist(draft, &context);
            }
            Step::CommandOpenInputs { draft } => {
                let mut context = Self::draft_context(&draft);
                context.push(pair("changes things", "yes"));
                self.mode = self.links_checklist(draft, &context);
            }
            Step::EditRun { app, tool } => {
                let current = crate::spec::lookup(&settings, &app, &tool);
                let words = match &current {
                    Some(current) if current.shown() == text => current.run.clone(),
                    _ => crate::spec::command_words(&text),
                };
                let title = format!("Edit {}", crate::core::exposed_name(&app, &tool));
                let draft = CommandSpec::draft(&app, words.clone());
                let problem = actions::program_problem(draft.program()).or_else(|| {
                    draft
                        .risk()
                        .runs_code
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

                let about = current.and_then(|current| current.description).unwrap_or_default();
                self.mode = input(
                    &title,
                    vec![pair("command", draft.shown())],
                    "what it does",
                    &about,
                    Step::EditAbout { app, tool, words },
                );
            }
            Step::EditAbout { app, tool, words } => {
                let reads = crate::spec::lookup(&settings, &app, &tool).is_some_and(|current| current.read_only);
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
                let current = crate::spec::lookup(&settings, &app, &tool);
                let edited = current.clone().map_or_else(
                    || CommandSpec::draft(&app, words.clone()),
                    |current| CommandSpec { run: words.clone(), ..current },
                );
                let draft = CommandSpec { read_only: text == "reads", ..edited }.honest();
                let asked_before = current.is_some_and(|current| current.run == words && current.risk().needs_person());

                if draft.risk().needs_person() && !asked_before {
                    self.mode = open_inputs_confirm(
                        &words.join(" "),
                        &draft,
                        Step::EditConfirmed { app, tool, words, description },
                    );
                    return;
                }

                self.applied(
                    actions::edit_tool(&self.config, &settings, &app, &tool, &words, &description, draft.read_only),
                    done,
                )
                .await;
            }
            Step::EditConfirmed { app, tool, words, description } => {
                let done = format!("Saved {}", crate::core::exposed_name(&app, &tool));
                self.applied(
                    actions::edit_tool(&self.config, &settings, &app, &tool, &words, &description, false),
                    done,
                )
                .await;
            }
            Step::CommandLinks { draft } => {
                self.busy(terminal, &format!("Sharing {}…", draft.app));
                let approved: Vec<String> = self
                    .snap
                    .links
                    .iter()
                    .filter(|info| !info.clients.is_empty())
                    .map(|info| info.name.clone())
                    .collect();
                self.mode = match actions::add_command(&self.config, &settings, &draft, &picked, &approved).await {
                    Ok(shared) => message(
                        if shared.problem.is_none() { "Added" } else { "Added, but not shared yet" },
                        crate::explain::shared(&shared),
                    ),
                    Err(error) => message("Couldn't add it", vec![error.message]),
                };
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

    pub(super) async fn find_servers(&mut self, settings: &crate::config::Settings, terminal: &mut DefaultTerminal) {
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
