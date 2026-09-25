use super::theme::{self, cut, fit};
use super::{App, Check, ClientRow, DetailRow, ListRow, Mode, TABS, Tab, TunnelRow, plural};
use crate::core::now_ms;
use crate::links::ServerState;
use crate::snapshot::{Badge, LinkInfo, ToolRow, Verdict};
use crate::tunnels::Provider;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

type Spans = Vec<Span<'static>>;

fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn width_of(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn spread(left: Spans, right: Spans, width: usize) -> Line<'static> {
    let gap = width.saturating_sub(width_of(&left) + width_of(&right)).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

fn key_hint(key: &str, label: &str) -> Spans {
    vec![span(key, theme::bold()), span(format!(" {label}"), theme::dim())]
}

fn keys(pairs: &[(&str, &str)]) -> Spans {
    let mut spans = Vec::new();

    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(span("   ", theme::dim()));
        }
        spans.extend(key_hint(key, label));
    }

    spans
}

fn heading(text: &str) -> Span<'static> {
    span(text.to_uppercase(), theme::dim())
}

fn blank() -> Line<'static> {
    Line::default()
}

fn text_area(area: Rect) -> Rect {
    Rect { x: area.x + 1, width: area.width.saturating_sub(3), ..area }
}

fn usable(area: Rect) -> usize {
    usize::from(area.width).saturating_sub(3)
}

fn put(buffer: &mut Buffer, area: Rect, row: u16, line: &Line<'_>) {
    if row < area.height {
        buffer.set_line(area.x, area.y + row, line, area.width);
    }
}

struct Rows {
    lines: Vec<Line<'static>>,
    selected: Option<(usize, usize)>,
}

impl Rows {
    fn new() -> Self {
        Self { lines: Vec::new(), selected: None }
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn pick(&mut self, line: Line<'static>, chosen: bool) {
        if chosen {
            self.selected = Some((self.lines.len(), 1));
        }
        self.lines.push(line);
    }

    fn extend_selection(&mut self, line: Line<'static>) {
        if let Some((start, count)) = self.selected.filter(|(start, count)| start + count == self.lines.len()) {
            self.selected = Some((start, count + 1));
        }
        self.lines.push(line);
    }
}

fn render_rows(buffer: &mut Buffer, area: Rect, rows: &Rows, focused: bool) {
    let height = usize::from(area.height);
    let total = rows.lines.len();

    if height == 0 {
        return;
    }

    let fits = total <= height;
    let window = if fits { height } else { height.saturating_sub(2).max(1) };
    let (start, count) = rows.selected.unwrap_or((0, 1));
    let mut offset = 0;

    if !fits {
        offset = (start + count).saturating_sub(window).min(start);
        offset = offset.min(total.saturating_sub(window));
    }

    let above = offset;
    let below = total.saturating_sub(offset + window);
    let body = if fits { area } else { Rect { y: area.y + 1, height: area.height.saturating_sub(2), ..area } };
    let content = Rect { x: area.x + 1, width: area.width.saturating_sub(3), ..body };

    if !fits {
        let marker = |count: usize, arrow: &str| {
            Line::from(span(if count == 0 { String::new() } else { format!("{arrow} {count} more") }, theme::faint()))
        };
        put(buffer, text_area(area), 0, &marker(above, "↑"));
        put(buffer, text_area(area), area.height - 1, &marker(below, "↓"));
    }

    for (index, line) in rows.lines.iter().skip(offset).take(window).enumerate() {
        let row = u16::try_from(index).unwrap_or(u16::MAX);
        let absolute = offset + index;
        let chosen = rows.selected.is_some_and(|(start, count)| absolute >= start && absolute < start + count);

        if chosen {
            let full = Rect { x: body.x, y: body.y + row, width: body.width.saturating_sub(2), height: 1 };
            if focused {
                theme::glow(buffer, full);
            } else {
                buffer.set_style(full, Style::new().bg(theme::SHADOW));
            }
            buffer.set_line(
                body.x,
                body.y + row,
                &Line::from(span("▌", if focused { theme::lamp() } else { theme::dim() })),
                1,
            );
        }

        buffer.set_line(content.x, content.y + row, line, content.width);
    }

    if !fits {
        let track = body.height;
        let thumb = u16::try_from((usize::from(track) * window / total.max(1)).max(1)).unwrap_or(1);
        let top = u16::try_from(usize::from(track - thumb) * offset / total.saturating_sub(window).max(1)).unwrap_or(0);
        let x = area.right().saturating_sub(1);

        for y in 0..track {
            let (symbol, style) = if y >= top && y < top + thumb {
                ("┃", theme::lamp())
            } else {
                ("│", Style::new().fg(theme::RULE))
            };
            if let Some(cell) = buffer.cell_mut((x, body.y + y)) {
                cell.set_symbol(symbol).set_style(style);
            }
        }
    }
}

pub fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, Style::new().bg(theme::NIGHT).fg(theme::TEXT));

    if area.width < 80 || area.height < 20 {
        let text =
            format!("porchlight needs a window of at least 80 × 20. This one is {} × {}.", area.width, area.height);
        frame.render_widget(
            Paragraph::new(text).style(theme::dim()).wrap(Wrap { trim: true }),
            area.inner(ratatui::layout::Margin::new(1, 1)),
        );
        return;
    }

    let pending = !app.snap.pending.is_empty() && app.tab != Tab::Clients;
    let [header, _, tabs, rule, body, request, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(u16::from(pending)),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area.inner(ratatui::layout::Margin::new(1, 0)));
    let buffer = frame.buffer_mut();
    let width = usize::from(header.width);

    put(buffer, text_area(header), 0, &status_line(app, usable(header)));
    put(buffer, tabs, 0, &tab_line(app));
    put(buffer, rule, 0, &Line::from(span("─".repeat(width), Style::new().fg(theme::RULE))));
    let body = Rect { y: body.y + 1, height: body.height.saturating_sub(1), ..body };

    match app.tab {
        Tab::Links if app.snap.live.is_none() && app.snap.links.is_empty() => first_run(app, buffer, body),
        Tab::Links => links(app, buffer, body),
        Tab::Apps => apps(app, buffer, body),
        Tab::Clients => clients(app, buffer, body),
        Tab::Logs => logs(app, buffer, body),
        Tab::Tunnel => tunnel(app, buffer, body),
    }

    if let Some(first) = app.snap.pending.first().filter(|_| pending) {
        buffer.set_style(request, Style::new().bg(theme::LAMP));
        let ink = Style::new().fg(theme::NIGHT);
        let left = vec![
            span(format!("◉ {} wants {}", first.client, first.link), ink.add_modifier(Modifier::BOLD)),
            span(
                format!(
                    "{} · {} left{}",
                    first.verified.as_ref().map_or(" · unverified".to_owned(), |host| format!(" · ✓ {host}")),
                    first.left,
                    if app.snap.pending.len() > 1 {
                        format!(" · +{} more", app.snap.pending.len() - 1)
                    } else {
                        String::new()
                    }
                ),
                ink,
            ),
        ];
        let right = vec![
            span("a", ink.add_modifier(Modifier::BOLD)),
            span(" approve in browser   ", ink),
            span("c", ink.add_modifier(Modifier::BOLD)),
            span(" show a code   ", ink),
            span("x", ink.add_modifier(Modifier::BOLD)),
            span(" deny ", ink),
        ];
        let mut left_padded = vec![span(" ", ink)];
        left_padded.extend(left);
        put(buffer, request, 0, &spread(left_padded, right, usize::from(request.width).saturating_sub(2)));
    }

    if let Some(job) = app.job.as_ref().filter(|job| job.hidden) {
        let step = job.progress.borrow().clone();
        put(
            buffer,
            text_area(status),
            0,
            &Line::from(vec![
                span("◌ ", theme::lamp()),
                span(format!("{} · {step} · {} s", job.title, job.since.elapsed().as_secs()), theme::secondary()),
            ]),
        );
    } else if !app.status.is_empty() {
        put(buffer, text_area(status), 0, &Line::from(span(app.status.clone(), theme::secondary())));
    }

    put(buffer, text_area(footer), 0, &spread(hints(app), keys(&[("?", "all keys"), ("q", "quit")]), usable(footer)));
    overlay(app, frame);
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    let running = app.snap.live.is_some();
    let how = if running && !app.snap.service.running { " · until you log out" } else { "" };
    let left = vec![
        span("porchlight  ", theme::lamp().add_modifier(Modifier::BOLD)),
        if running { span("● sharing", theme::state()) } else { span("○ not sharing", theme::dim()) },
        span(how, theme::dim()),
    ];
    let tunnel = Provider::named(&app.snap.tunnel)
        .map_or_else(|| "your own tunnel".to_owned(), |provider| provider.name().to_owned());
    let address = app
        .snap
        .live
        .as_ref()
        .map(|live| live.public_url.trim_start_matches("https://").to_owned())
        .unwrap_or_default();
    let right = vec![span(format!("{tunnel}  "), theme::dim()), span(address, theme::text())];

    spread(left, right, width)
}

fn tab_line(app: &App) -> Line<'static> {
    let problems =
        app.snap.apps.iter().filter(|info| info.problem.is_some() || info.state == Some(ServerState::Refused)).count()
            + app.snap.file_problems.len();
    let spans: Spans = TABS
        .iter()
        .flat_map(|tab| {
            let badge = match tab {
                Tab::Apps if problems > 0 => format!(" {problems}"),
                Tab::Clients if !app.snap.pending.is_empty() => format!(" {}", app.snap.pending.len()),
                _ => String::new(),
            };
            let selected = *tab == app.tab;
            let mut spans = vec![if selected {
                span(format!(" {}{badge} ", tab.title()), theme::lit_block())
            } else {
                span(format!(" {}", tab.title()), theme::dim())
            }];
            if !selected && !badge.is_empty() {
                spans.push(span(badge, theme::lamp()));
                spans.push(span(" ", theme::dim()));
            } else if !selected {
                spans.push(span(" ", theme::dim()));
            }
            spans.push(span("  ", theme::dim()));
            spans
        })
        .collect();

    Line::from(spans)
}

fn context_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.tab {
        Tab::Links if app.snap.live.is_none() && app.snap.links.is_empty() => {
            vec![("↑↓", "choose"), ("enter", "start sharing")]
        }
        Tab::Links if app.pane_detail => match app.detail_rows().get(app.detail_cursor) {
            Some(DetailRow::Group { .. }) => vec![
                ("space", "on/off"),
                ("enter", "fold"),
                ("s", "links"),
                ("d", "remove app"),
                ("/", "filter"),
                ("z", "fold all"),
                ("←", "links"),
            ],
            _ => vec![
                ("space", "on/off"),
                ("t", "try"),
                ("u", "edit"),
                ("d", "remove"),
                ("/", "filter"),
                ("z", "fold all"),
                ("←", "links"),
                ("+", "add"),
            ],
        },
        Tab::Links => match app.list_rows().get(app.list_cursor) {
            Some(ListRow::Unshared(_)) => {
                vec![("enter", "put on links"), ("d", "remove"), ("+", "add"), ("n", "new link")]
            }
            Some(ListRow::NewLink) => vec![("enter", "new link"), ("+", "add")],
            _ => vec![
                ("→", "tools"),
                ("y", "copy address"),
                ("e", "apps on link"),
                ("n", "new link"),
                ("d", "delete"),
                ("+", "add"),
            ],
        },
        Tab::Apps => vec![
            ("enter", "tools"),
            ("space", "on/off"),
            ("/", "search"),
            ("+", "add"),
            ("t", "try"),
            ("s", "links"),
            ("d", "remove"),
            ("o", "settings file"),
        ],
        Tab::Clients => match app.client_rows().get(app.clients_cursor) {
            Some(ClientRow::Pending(_)) => vec![("a", "approve in browser"), ("c", "show a code"), ("x", "deny")],
            Some(ClientRow::Client(_)) => {
                vec![("r", "revoke"), ("l", "its calls"), ("n", "new token"), ("R", "revoke everyone")]
            }
            _ => vec![("enter", "new token"), ("R", "revoke everyone")],
        },
        Tab::Logs if app.logs.paused.is_some() => vec![
            ("p", "resume"),
            ("enter", "details"),
            ("/", "search"),
            ("l", "link"),
            ("c", "client"),
            ("o", "only refusals & changes"),
        ],
        Tab::Logs => vec![
            ("enter", "details"),
            ("/", "search"),
            ("l", "link"),
            ("c", "client"),
            ("o", "only refusals & changes"),
            ("p", "pause"),
        ],
        Tab::Tunnel => vec![
            ("enter", "switch"),
            ("y", "copy address"),
            ("S", if app.snap.live.is_some() { "stop sharing" } else { "start sharing" }),
            ("w", "local page"),
            ("↑↓", "choose"),
        ],
    }
}

fn hints(app: &App) -> Spans {
    if app.searching.is_some() {
        return keys(&[("type", "to filter"), ("enter", "keep it"), ("esc", "clear")]);
    }

    let shown: Vec<(&str, &str)> = context_keys(app).into_iter().take(4).collect();

    keys(&shown)
}

fn first_run(app: &App, buffer: &mut Buffer, area: Rect) {
    let text = text_area(area);
    let intro = [
        blank(),
        Line::from(span("Share tools on this computer. You approve every client.", theme::bold())),
        blank(),
        Line::from(heading("how clients reach it")),
    ];

    for (row, line) in intro.iter().enumerate() {
        put(buffer, text, u16::try_from(row).unwrap_or(0), line);
    }

    let mut rows = Rows::new();
    let options = [("OpenTunnel", "free · installs now"), ("Your own https address", "a tunnel you already run")];

    for (index, (label, hint)) in options.iter().enumerate() {
        let chosen = (index == 1) == app.first_run_own;
        rows.pick(
            Line::from(vec![
                span(if chosen { "(●) " } else { "( ) " }, if chosen { theme::lamp() } else { theme::dim() }),
                span(fit(label, 28), if chosen { theme::bold() } else { theme::text() }),
                span(*hint, theme::dim()),
            ]),
            chosen,
        );
    }

    render_rows(buffer, Rect { y: area.y + 5, height: 2, ..area }, &rows, true);
    put(buffer, text, 8, &Line::from(keys(&[("enter", "start sharing")])));
}

fn state_spans(link: &LinkInfo) -> Spans {
    if !link.dangerous.is_empty() && link.state == Some(ServerState::Live) && link.composed {
        return vec![span(
            format!("▲ {} refused · {} of {} tools on", link.refused_app, link.on, link.total),
            theme::danger(),
        )];
    }

    match link.state {
        Some(ServerState::Live) => vec![
            span("● ", theme::state()),
            span(format!("live · {} of {} tools on", link.on, link.total.max(link.on)), theme::state()),
        ],
        Some(ServerState::Refused) => vec![span("▲ refused", theme::danger())],
        Some(ServerState::Waiting) => vec![span("◌ waiting for the server · retrying every 3 s", theme::secondary())],
        Some(ServerState::Starting) => vec![span("◌ starting…", theme::secondary())],
        None => vec![span("○ not running", theme::dim())],
    }
}

fn links(app: &App, buffer: &mut Buffer, area: Rect) {
    let left_width = 32.min(area.width / 3);
    let [left, divider, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Length(1), Constraint::Min(20)]).areas(area);

    for y in divider.top()..divider.bottom() {
        if let Some(cell) = buffer.cell_mut((divider.x, y)) {
            cell.set_symbol("│").set_style(Style::new().fg(theme::RULE));
        }
    }

    let inner = usable(left);
    put(buffer, text_area(left), 0, &Line::from(heading(&format!("links · {}", app.snap.links.len()))));
    let mut rows = Rows::new();

    for (index, row) in app.list_rows().iter().enumerate() {
        let chosen = index == app.list_cursor;
        match row {
            ListRow::Link(name) => {
                let Some(link) = app.snap.links.iter().find(|link| link.name == *name) else { continue };
                let state = if link.dangerous.is_empty() { link.state } else { Some(ServerState::Refused) };
                let (dot, style, tail, tail_style) = match state {
                    Some(ServerState::Refused) => ("▲ ", theme::danger(), "refused".to_owned(), theme::danger()),
                    Some(ServerState::Waiting | ServerState::Starting) => {
                        ("◌ ", theme::secondary(), "waiting".to_owned(), theme::secondary())
                    }
                    Some(ServerState::Live) => {
                        ("● ", theme::state(), format!("{} of {}", link.on, link.total.max(link.on)), theme::dim())
                    }
                    None => ("○ ", theme::dim(), "off".to_owned(), theme::faint()),
                };
                let name_style = if chosen { theme::bold() } else { theme::text() };
                rows.pick(
                    spread(
                        vec![span(dot, style), span(cut(name, inner.saturating_sub(12)), name_style)],
                        vec![span(tail, tail_style)],
                        inner,
                    ),
                    chosen,
                );
            }
            ListRow::NewLink => {
                rows.push(blank());
                rows.pick(spread(vec![span("+ new link", theme::dim())], vec![span("n", theme::dim())], inner), chosen);
                if app.list_rows().iter().any(|row| matches!(row, ListRow::Unshared(_))) {
                    rows.push(blank());
                    rows.push(Line::from(heading(&format!(
                        "not shared · {}",
                        app.list_rows().iter().filter(|row| matches!(row, ListRow::Unshared(_))).count()
                    ))));
                }
            }
            ListRow::Unshared(name) => {
                let off = app.snap.apps.iter().any(|info| info.name == *name && !info.on);
                rows.pick(
                    Line::from(vec![
                        span("○ ", theme::faint()),
                        span(name.clone(), theme::dim()),
                        span(if off { "  off" } else { "" }, theme::faint()),
                    ]),
                    chosen,
                );
            }
        }
    }

    let list_area = Rect { y: left.y + 2, height: left.height.saturating_sub(2), ..left };
    render_rows(buffer, list_area, &rows, !app.pane_detail);

    match app.list_rows().get(app.list_cursor) {
        Some(ListRow::Link(_)) => {
            if let Some(link) = app.selected_link() {
                link_detail(app, link, buffer, right);
            }
        }
        Some(ListRow::Unshared(name)) => {
            let lines = [
                Line::from(span(name.clone(), theme::bold())),
                blank(),
                Line::from(span("Not on any link yet, so no client can use it.", theme::dim())),
                blank(),
                Line::from(vec![span("enter", theme::bold()), span(" pick links for it", theme::dim())]),
            ];
            for (row, line) in lines.iter().enumerate() {
                put(buffer, text_area(right), u16::try_from(row).unwrap_or(0), line);
            }
        }
        _ => {
            let lines = [
                Line::from(span("A link is an address you give to clients.", theme::text())),
                blank(),
                Line::from(span(
                    "Each MCP server gets its own. Command-line tools go on links you name,",
                    theme::dim(),
                )),
                Line::from(span("like home or work, and one tool can be on several.", theme::dim())),
            ];
            for (row, line) in lines.iter().enumerate() {
                put(buffer, text_area(right), u16::try_from(row).unwrap_or(0), line);
            }
        }
    }
}

fn badge(tool: &ToolRow) -> Span<'static> {
    let (text, style) = match tool.badge {
        Badge::Changes => ("changes things", theme::secondary()),
        Badge::ReadOnly => ("read-only", theme::dim()),
        Badge::Resource => ("resource", theme::dim()),
        Badge::RunsCode => ("▲ runs code", theme::danger()),
        Badge::Plain => ("", theme::dim()),
    };

    span(format!("{text:>14}"), style)
}

fn inputs_of(tool: &ToolRow) -> String {
    if tool.inputs.is_empty() {
        return String::new();
    }

    format!(" {}", tool.inputs.iter().map(|input| format!("{{{input}}}")).collect::<Vec<_>>().join(" "))
}

fn link_detail(app: &App, link: &LinkInfo, buffer: &mut Buffer, area: Rect) {
    let width = usable(area);
    let mut header = vec![spread(vec![span(link.name.clone(), theme::bold())], state_spans(link), width), blank()];
    let address = link.address.clone().unwrap_or_else(|| "starts when porchlight runs".to_owned());
    header.push(spread(
        vec![span(
            address,
            if link.address.is_some() { theme::text().add_modifier(Modifier::UNDERLINED) } else { theme::dim() },
        )],
        key_hint("y", "copy"),
        width,
    ));
    let clients = if link.clients.is_empty() {
        "no clients yet".to_owned()
    } else {
        let shown: Vec<String> = link.clients.iter().take(3).cloned().collect();
        let more = link.clients.len().saturating_sub(3);
        format!(
            "{} · {}{}",
            plural(link.clients.len(), "client"),
            shown.join(", "),
            if more > 0 { format!(" +{more}") } else { String::new() }
        )
    };
    let secondary = if link.source.is_empty() { clients } else { format!("{} · {clients}", link.source) };
    header.push(spread(
        vec![span(cut(&secondary, width.saturating_sub(24)), theme::dim())],
        if link.composed { key_hint("e", "apps on this link") } else { vec![] },
        width,
    ));
    header.push(blank());

    if !link.dangerous.is_empty() {
        let app_name = link.refused_app.clone();
        let others = link
            .groups
            .iter()
            .find(|group| group.name == link.refused_app)
            .map_or(0, |group| group.tools.len())
            .saturating_sub(link.dangerous.len());
        header.extend([
            Line::from(vec![
                span("▎ ", theme::danger()),
                span(
                    format!(
                        "Not shared yet: {} of {app_name}'s tools can run code on this computer.",
                        link.dangerous.len()
                    ),
                    theme::bold(),
                ),
            ]),
            Line::from(vec![
                span("▎ ", theme::danger()),
                span("A client approved for this link could use them to do anything you can.", theme::dim()),
            ]),
            Line::from(span("▎", theme::danger())),
            Line::from(vec![span("▎ ", theme::danger()), span("Fix it either way:", theme::text())]),
            Line::from(vec![
                span("▎   ", theme::danger()),
                span("f  ", theme::bold()),
                span(format!("turn off {}", link.dangerous.join(", ")), theme::text()),
                span(
                    if others > 0 {
                        format!("  · {app_name} shares its other {others}")
                    } else {
                        format!("  · {app_name} stays on its link, with nothing on")
                    },
                    theme::dim(),
                ),
            ]),
            Line::from(vec![
                span("▎   ", theme::danger()),
                span("D  ", theme::bold()),
                span(format!("allow dangerous tools for {app_name}"), theme::text()),
                span("  · asks you to confirm", theme::dim()),
            ]),
            blank(),
        ]);
    }

    let total: usize = link.groups.iter().map(|group| group.tools.len()).sum();
    header.push(spread(vec![heading(&format!("tools · {total}"))], keys(&[("z", "fold all"), ("/", "filter")]), width));

    let editing_tools = app.searching == Some(super::Search::Tools);
    if !app.tool_filter.is_empty() || editing_tools {
        header.push(Line::from(vec![
            span("/ ", theme::lamp()),
            span(app.tool_filter.clone(), theme::text()),
            span(if editing_tools { "█" } else { "" }, theme::text()),
            span(
                format!(
                    "   {} match · esc clears",
                    app.detail_rows().iter().filter(|row| matches!(row, DetailRow::Tool { .. })).count()
                ),
                theme::dim(),
            ),
        ]));
    }

    for (row, line) in header.iter().enumerate() {
        put(buffer, text_area(area), u16::try_from(row).unwrap_or(0), line);
    }

    let used = u16::try_from(header.len()).unwrap_or(0);
    let list_area = Rect { y: area.y + used, height: area.height.saturating_sub(used), ..area };

    if link.groups.iter().all(|group| group.tools.is_empty()) {
        let text = match link.state {
            Some(ServerState::Waiting | ServerState::Starting) => "Its tools show up once the server answers.",
            None => "Its tools show up once porchlight runs.",
            _ => "No tools.",
        };
        put(buffer, text_area(list_area), 0, &Line::from(span(text, theme::dim())));
        return;
    }

    let name_width = link
        .groups
        .iter()
        .flat_map(|group| &group.tools)
        .map(|tool| tool.exposed.width() + inputs_of(tool).width())
        .max()
        .unwrap_or(10)
        .clamp(10, 34)
        + 2;
    let about_width = width.saturating_sub(name_width + 7 + 14);
    let mut rows = Rows::new();

    for (index, row) in app.detail_rows().iter().enumerate() {
        let chosen = app.pane_detail && index == app.detail_cursor;
        match row {
            DetailRow::Group { app: name } => {
                let Some(group) = link.groups.iter().find(|group| group.name == *name) else { continue };
                let off = group.tools.iter().filter(|tool| !tool.on).count();
                let folded = app.folded.contains(&format!("{}/{name}", link.name)) && app.tool_filter.is_empty();
                let mut right = group.kind.clone();
                if group.everywhere {
                    right.push_str(" · on every link");
                } else if !group.elsewhere.is_empty() {
                    right = format!("{right} · also on {}", group.elsewhere.join(", "));
                }
                if !index.eq(&0) {
                    rows.push(blank());
                }
                rows.pick(
                    spread(
                        vec![
                            span(if folded { "▸ " } else { "▾ " }, theme::dim()),
                            span(name.clone(), theme::bold()),
                            span(
                                format!(
                                    "  {}{}",
                                    plural(group.tools.len(), "tool"),
                                    if off > 0 { format!(" · {off} off") } else { String::new() }
                                ),
                                theme::dim(),
                            ),
                        ],
                        vec![span(right, theme::dim())],
                        width,
                    ),
                    chosen,
                );
            }
            DetailRow::Tool { app: name, tool } => {
                let Some(tool) = link
                    .groups
                    .iter()
                    .find(|group| group.name == *name)
                    .and_then(|group| group.tools.iter().find(|row| row.tool == *tool))
                else {
                    continue;
                };
                let (switch, style) = if tool.on { ("  on   ", theme::state()) } else { ("  off  ", theme::dim()) };
                let name_style = if !tool.on {
                    theme::dim().add_modifier(Modifier::CROSSED_OUT)
                } else if chosen {
                    theme::bold()
                } else {
                    theme::text()
                };
                let room = name_width - 2;
                let shown_name = cut(&tool.exposed, room);
                let shown_inputs = cut(&inputs_of(tool), room.saturating_sub(shown_name.width()));
                let pad = name_width.saturating_sub(shown_name.width() + shown_inputs.width());
                rows.pick(
                    Line::from(vec![
                        span(switch, style),
                        span(shown_name, name_style),
                        span(format!("{shown_inputs}{}", " ".repeat(pad)), theme::faint()),
                        span(fit(&cut(&tool.about, about_width.saturating_sub(1)), about_width), theme::dim()),
                        badge(tool),
                    ]),
                    chosen,
                );
            }
        }
    }

    render_rows(buffer, list_area, &rows, app.pane_detail);
}

fn apps(app: &App, buffer: &mut Buffer, area: Rect) {
    let width = usable(area);
    put(
        buffer,
        text_area(area),
        0,
        &spread(
            vec![heading(&format!("apps · {}", app.snap.apps.len()))],
            keys(&[("/", "search"), ("+", "add app")]),
            width,
        ),
    );
    let editing = app.searching == Some(super::Search::Apps);
    let shown = app.app_rows();

    if !app.apps_filter.is_empty() || editing {
        put(
            buffer,
            text_area(area),
            1,
            &Line::from(vec![
                span("/ ", theme::lamp()),
                span(app.apps_filter.clone(), theme::text()),
                span(if editing { "█" } else { "" }, theme::text()),
                span(
                    format!("   {} of {} · apps and their tools · esc clears", shown.len(), app.snap.apps.len()),
                    theme::dim(),
                ),
            ]),
        );
    }

    put(
        buffer,
        text_area(area),
        2,
        &Line::from(span(
            format!("{}{}{}{}", fit("NAME", 16), fit("KIND", 26), fit("ON LINKS", 22), "STATE"),
            theme::dim(),
        )),
    );
    let mut rows = Rows::new();

    for (index, info) in shown.iter().enumerate() {
        let chosen = index == app.apps_cursor;
        let links = if info.everywhere {
            "every link (*)".to_owned()
        } else if !info.command {
            "its own link".to_owned()
        } else if info.links.is_empty() {
            "not on a link".to_owned()
        } else {
            info.links.join(", ")
        };
        let refused = info.problem.as_ref().is_some_and(|problem| problem.starts_with("refused: "));
        let state = if refused {
            span("▲ refused · runs code", theme::danger())
        } else if info.problem.is_some() {
            span("▲ problem", theme::danger())
        } else if !info.on {
            span("○ off", theme::dim())
        } else if info.state == Some(ServerState::Refused) {
            span("▲ refused · tools that run code", theme::danger())
        } else if matches!(info.state, Some(ServerState::Waiting | ServerState::Starting)) {
            span("◌ waiting · retrying every 3 s", theme::secondary())
        } else if info.links.is_empty() && !info.everywhere {
            span("○ not shared", theme::dim())
        } else if app.snap.live.is_none() {
            span("○ not running", theme::dim())
        } else {
            span(if info.command { "● live".to_owned() } else { format!("● live · {}", info.detail) }, theme::state())
        };
        rows.pick(
            Line::from(vec![
                span(fit(&info.name, 16), if chosen { theme::bold() } else { theme::text() }),
                span(fit(&info.kind, 26), theme::dim()),
                span(
                    fit(&links, 22),
                    if info.links.is_empty() && !info.everywhere { theme::dim() } else { theme::text() },
                ),
                state,
            ]),
            chosen,
        );

        let matched = app.matched_tools(info);

        if !matched.is_empty() && !info.name.to_lowercase().contains(&app.apps_filter.to_lowercase()) {
            rows.extend_selection(Line::from(span(
                format!("{}└ has {}", " ".repeat(16), cut(&matched.join(", "), width.saturating_sub(22))),
                theme::secondary(),
            )));
        }

        if let Some(problem) = &info.problem {
            let text = if refused {
                format!(
                    "can run commands or code · fix it on the {} link with f or D",
                    info.links.first().cloned().unwrap_or_default()
                )
            } else {
                problem.clone()
            };
            rows.extend_selection(Line::from(span(
                format!("{}└ {}", " ".repeat(16), cut(&text, width.saturating_sub(20))),
                theme::danger(),
            )));
        }
    }

    let problems = &app.snap.file_problems;
    let footer_height = 5 + u16::try_from(problems.len()).unwrap_or(0);
    let list_area = Rect { y: area.y + 3, height: area.height.saturating_sub(4 + footer_height), ..area };
    render_rows(buffer, list_area, &rows, true);
    let bottom = text_area(Rect { y: area.bottom().saturating_sub(footer_height), height: footer_height, ..area });
    put(buffer, bottom, 0, &spread(vec![heading("settings file")], key_hint("o", "open in editor"), width));
    put(
        buffer,
        bottom,
        2,
        &Line::from(vec![
            span(cut(&app.config.path.display().to_string(), width.saturating_sub(40)), theme::text()),
            span("  · edits apply as soon as you save", theme::dim()),
        ]),
    );

    for (index, problem) in problems.iter().enumerate() {
        put(
            buffer,
            bottom,
            3 + u16::try_from(index).unwrap_or(0),
            &Line::from(vec![span("▲ ", theme::danger()), span(cut(problem, width.saturating_sub(4)), theme::text())]),
        );
    }
}

fn clients(app: &App, buffer: &mut Buffer, area: Rect) {
    let width = usable(area);
    let mut rows = Rows::new();
    let client_rows = app.client_rows();

    if !app.snap.pending.is_empty() {
        rows.push(Line::from(span(format!("WAITING FOR YOU · {}", app.snap.pending.len()), theme::lamp())));
        rows.push(blank());
    }

    for (index, row) in client_rows.iter().enumerate() {
        let chosen = index == app.clients_cursor;
        match row {
            ClientRow::Pending(id) => {
                let Some(pending) = app.snap.pending.iter().find(|pending| pending.id == *id) else { continue };
                let verified = match &pending.verified {
                    Some(host) if *host == pending.client => span("✓ verified", theme::state()),
                    Some(host) => span(format!("✓ verified {host}"), theme::state()),
                    None => span("unverified name", theme::danger()),
                };
                let asking = format!("{} wants {}", pending.client, pending.link);
                rows.pick(
                    spread(
                        vec![span(fit(&asking, 30), theme::bold()), span("  ", theme::dim()), verified],
                        vec![span(format!("asked {} · {} left", pending.asked, pending.left), theme::dim())],
                        width,
                    ),
                    chosen,
                );
                let mut gets = vec![span("will get  ", theme::dim())];
                for (tool, changes) in pending.tools.iter().take(5) {
                    gets.push(span(tool.clone(), theme::text()));
                    gets.push(span(if *changes { " changes  " } else { "  " }, theme::secondary()));
                }
                if pending.tools.len() > 5 {
                    gets.push(span(format!("+{}", pending.tools.len() - 5), theme::text()));
                }
                if pending.tools.is_empty() {
                    gets.push(span("no tools yet", theme::dim()));
                }
                rows.extend_selection(Line::from(gets));
                rows.push(blank());
            }
            ClientRow::Client(id) => {
                if index == app.snap.pending.len() {
                    rows.push(blank());
                    rows.push(spread(
                        vec![heading(&format!("approved · {}", app.snap.clients.len()))],
                        key_hint("R", "revoke everyone"),
                        width,
                    ));
                    rows.push(blank());
                    rows.push(Line::from(span(
                        format!(
                            "{}{}{}{}{}",
                            fit("NAME", 30),
                            fit("LINK", 12),
                            fit("KIND", 12),
                            fit("LAST USED", 18),
                            "APPROVED"
                        ),
                        theme::dim(),
                    )));
                }
                let Some(client) = app.snap.clients.iter().find(|client| client.id == *id) else { continue };
                rows.pick(
                    Line::from(vec![
                        span(fit(&client.name, 30), if chosen { theme::bold() } else { theme::text() }),
                        span(fit(&client.link, 12), theme::text()),
                        span(fit(if client.token { "token" } else { "client" }, 12), theme::dim()),
                        span(fit(&client.last_used, 18), theme::text()),
                        span(client.since.clone(), theme::dim()),
                    ]),
                    chosen,
                );
            }
            ClientRow::NewToken => {
                if app.snap.clients.is_empty() {
                    rows.push(blank());
                    rows.push(Line::from(span(
                        "No approved clients yet. Give a client a link from the Links tab, then approve it here.",
                        theme::dim(),
                    )));
                }
                rows.push(blank());
                rows.pick(
                    spread(
                        vec![span("+ new token for an automation", theme::dim())],
                        vec![span("n", theme::dim())],
                        width,
                    ),
                    chosen,
                );
            }
        }
    }

    render_rows(buffer, area, &rows, true);
}

fn logs(app: &App, buffer: &mut Buffer, area: Rect) {
    let width = usable(area);
    let text = text_area(area);
    let filter = |value: &Option<String>| value.clone().unwrap_or_else(|| "all".to_owned());
    let mut left = vec![heading("logs"), span(format!(" · {} today  ", app.snap.log_today), theme::dim())];
    match app.logs.paused {
        Some(kept) => {
            left.push(span("‖ paused", theme::secondary()));
            let new = app.snap.log.len().saturating_sub(kept);
            if new > 0 {
                left.push(span(format!(" · {new} new ↓"), theme::lamp()));
            }
        }
        None => left.push(span("● live", theme::state())),
    }
    let right = vec![
        span("link ", theme::dim()),
        span(filter(&app.logs.link), theme::text()),
        span("   client ", theme::dim()),
        span(filter(&app.logs.client), theme::text()),
        span("   only ", theme::dim()),
        span(if app.logs.only_changes { "refusals & changes" } else { "everything" }, theme::text()),
    ];
    put(buffer, text, 0, &spread(left, right, width));
    let mut top = 2;

    let editing_logs = app.searching == Some(super::Search::Logs);
    if !app.logs.search.is_empty() || editing_logs {
        let matches = app.log_rows().len();
        put(
            buffer,
            text,
            1,
            &Line::from(vec![
                span("/ ", theme::lamp()),
                span(app.logs.search.clone(), theme::text()),
                span(if editing_logs { "█" } else { "" }, theme::text()),
                span(format!("   {} · esc clears", plural(matches, "match")), theme::dim()),
            ]),
        );
        top = 3;
    }

    let what_width = width.saturating_sub(10 + 20 + 12 + 34);
    put(
        buffer,
        text,
        top,
        &Line::from(span(
            format!(
                "{}{}{}{}{}",
                fit("TIME", 10),
                fit("CLIENT", 20),
                fit("LINK", 12),
                fit("WHAT", what_width),
                "RESULT"
            ),
            theme::dim(),
        )),
    );
    let rows_all = app.log_rows();
    let mut rows = Rows::new();
    let mut day = String::new();
    let search = app.logs.search.to_lowercase();

    for (index, row) in rows_all.iter().enumerate() {
        if row.day != day {
            day.clone_from(&row.day);
            rows.push(Line::from(span(
                format!("─ {day} {}", "─".repeat(width.saturating_sub(day.width() + 3))),
                theme::faint(),
            )));
        }

        let chosen = index == app.logs_cursor;
        let (result, style) = match row.result {
            Verdict::Ok => ("ok", theme::state()),
            Verdict::Refused => ("refused", theme::danger()),
            Verdict::Error => ("error", theme::danger()),
            Verdict::Change => ("change", theme::secondary()),
            Verdict::Note => ("", theme::dim()),
        };
        let base = if row.result == Verdict::Change {
            theme::secondary()
        } else if chosen {
            theme::bold()
        } else {
            theme::text()
        };
        let what = cut(&row.what, what_width.saturating_sub(2));
        let mut what_spans = Vec::new();
        match (!search.is_empty()).then(|| what.to_lowercase().find(&search)).flatten() {
            Some(at) => {
                let end = at + search.len();
                what_spans.push(span(what.get(..at).unwrap_or_default().to_owned(), base));
                what_spans.push(span(what.get(at..end).unwrap_or_default().to_owned(), theme::lamp()));
                what_spans.push(span(what.get(end..).unwrap_or_default().to_owned(), base));
            }
            None => what_spans.push(span(what.clone(), base)),
        }
        what_spans.push(span(" ".repeat(what_width.saturating_sub(what.width())), base));
        let mut line = vec![
            span(fit(&row.time, 10), theme::dim()),
            span(fit(&row.client, 20), if row.client == "unknown" { theme::dim() } else { base }),
            span(fit(&row.link, 12), base),
        ];
        line.extend(what_spans);
        line.push(span(result, style));
        if !row.detail.is_empty() {
            line.push(span(
                format!("{}{}", if result.is_empty() { "" } else { " · " }, cut(&row.detail, 24)),
                theme::dim(),
            ));
        }
        rows.pick(Line::from(line), chosen);

        if chosen && app.logs.expanded {
            if let Some(input) = &row.input {
                rows.extend_selection(Line::from(vec![
                    span(format!("{}input   ", " ".repeat(10)), theme::dim()),
                    span(cut(input, width.saturating_sub(20)), theme::text()),
                ]));
            }
            if !row.detail.is_empty() {
                rows.extend_selection(Line::from(vec![
                    span(format!("{}detail  ", " ".repeat(10)), theme::dim()),
                    span(cut(&row.detail, width.saturating_sub(20)), theme::text()),
                ]));
            }
        }
    }

    if rows_all.is_empty() {
        rows.push(Line::from(span(
            if app.snap.log.is_empty() { "Nothing has happened yet." } else { "Nothing matches." },
            theme::dim(),
        )));
    }

    let list = Rect { y: area.y + top + 1, height: area.height.saturating_sub(top + 1), ..area };
    render_rows(buffer, list, &rows, true);
}

fn tunnel(app: &App, buffer: &mut Buffer, area: Rect) {
    let width = usable(area);
    let current_open = Provider::named(&app.snap.tunnel).is_some();
    let address = app.snap.live.as_ref().map(|live| live.public_url.clone());
    let mut rows = Rows::new();

    for (index, row) in App::tunnel_rows().iter().enumerate() {
        let chosen = index == app.tunnel_cursor;
        let active = (*row == TunnelRow::Open) == current_open;
        let (label, hint) = match row {
            TunnelRow::Open => ("OpenTunnel", "free, no account · installs if missing".to_owned()),
            TunnelRow::Own => (
                "Your own https address",
                if current_open {
                    "for a tunnel you already run, pointed at this computer".to_owned()
                } else if address.is_some() {
                    String::new()
                } else {
                    app.snap.tunnel.clone()
                },
            ),
        };
        rows.pick(
            Line::from(vec![
                span(if active { "(●) " } else { "( ) " }, if active { theme::lamp() } else { theme::dim() }),
                span(fit(label, 28), if active { theme::bold() } else { theme::text() }),
                span(hint, theme::dim()),
            ]),
            chosen,
        );
        if active && let Some(address) = &address {
            rows.extend_selection(Line::from(span(format!("    {address}"), theme::text())));
        }
        rows.push(blank());
    }

    let lines = [
        Line::from(heading("how clients reach this computer")),
        Line::from(span("Switching applies right away. Clients reconnect by themselves.", theme::dim())),
    ];
    for (row, line) in lines.iter().enumerate() {
        put(buffer, text_area(area), u16::try_from(row).unwrap_or(0), line);
    }
    render_rows(buffer, Rect { y: area.y + 3, height: 7, ..area }, &rows, true);

    let port = app.store.ports().map(|ports| ports.approval).unwrap_or_default();
    let service = if app.snap.live.is_some() && app.snap.service.running {
        vec![span("● sharing", theme::state()), span(" · in the background, starts at login", theme::dim())]
    } else if app.snap.live.is_some() {
        vec![span("● sharing", theme::state()), span(" · until you log out", theme::dim())]
    } else {
        vec![span("○ not sharing", theme::dim())]
    };
    let info = |label: &str, value: Spans, key: &str, action: &str| {
        let mut left = vec![span(fit(label, 16), theme::dim())];
        left.extend(value);
        let right = if key.is_empty() {
            vec![]
        } else {
            vec![span(key.to_owned(), theme::bold()), span(format!(" {}", fit(action, 15)), theme::dim())]
        };
        spread(left, right, width)
    };
    let bottom = [
        Line::from(heading("this computer")),
        blank(),
        info("porchlight", service, "S", if app.snap.live.is_some() { "stop sharing" } else { "start sharing" }),
        info(
            "local page",
            vec![
                span(format!("http://127.0.0.1:{port}"), theme::text()),
                span(" · only this computer can open it", theme::dim()),
            ],
            "w",
            "open in browser",
        ),
        info(
            "settings",
            vec![span(cut(&app.config.path.display().to_string(), width.saturating_sub(40)), theme::text())],
            "o",
            "open in editor",
        ),
        info(
            "history",
            vec![
                span(
                    cut(&crate::config::in_state_dir("state.db").display().to_string(), width.saturating_sub(42)),
                    theme::text(),
                ),
                span(" · logs kept 90 days", theme::dim()),
            ],
            "",
            "",
        ),
    ];
    for (row, line) in bottom.iter().enumerate() {
        put(buffer, text_area(area), 11 + u16::try_from(row).unwrap_or(0), line);
    }
}

fn popup(frame: &mut Frame, width: u16, height: u16, danger: bool) -> Rect {
    let area = frame.area();
    let buffer = frame.buffer_mut();

    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_fg(theme::FAINT).set_bg(theme::NIGHT);
            }
        }
    }

    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect { x: (area.width - width) / 2, y: (area.height - height) / 2, width, height };
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if danger { theme::DANGER } else { theme::BORDER }))
        .style(Style::new().bg(theme::NIGHT));
    let inner = block.inner(rect).inner(ratatui::layout::Margin::new(2, 1));
    frame.render_widget(block, rect);
    inner
}

fn fill(frame: &mut Frame, area: Rect, lines: &[Line<'static>]) {
    for (row, line) in lines.iter().enumerate() {
        put(frame.buffer_mut(), area, u16::try_from(row).unwrap_or(u16::MAX), line);
    }
}

fn title_line(title: &str, right: &str, width: usize, danger: bool) -> Line<'static> {
    spread(
        vec![span(title.to_owned(), if danger { theme::danger().add_modifier(Modifier::BOLD) } else { theme::bold() })],
        vec![span(right.to_owned(), theme::dim())],
        width,
    )
}

fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    if text.starts_with(' ') || text.width() <= width {
        return vec![Line::from(span(text.to_owned(), style))];
    }

    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split(' ') {
        if !current.is_empty() && current.width() + word.width() + 1 > width {
            lines.push(Line::from(span(std::mem::take(&mut current), style)));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    lines.push(Line::from(span(current, style)));
    lines
}

fn height_of(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(30).saturating_add(4)
}

fn context_lines(context: &[(String, String)]) -> Vec<Line<'static>> {
    context
        .iter()
        .map(|(label, value)| Line::from(vec![span(fit(label, 16), theme::dim()), span(value.clone(), theme::text())]))
        .collect()
}

fn changes(items: &[Check]) -> Vec<Line<'static>> {
    items
        .iter()
        .filter(|item| item.checked != item.start)
        .map(|item| {
            if item.checked {
                Line::from(vec![
                    span(" + ", theme::secondary()),
                    span(fit(&item.label, 12), theme::text()),
                    span(item.gain.clone(), theme::dim()),
                ])
            } else {
                Line::from(vec![
                    span(" − ", theme::danger()),
                    span(fit(&item.label, 12), theme::text()),
                    span(item.lose.clone(), theme::dim()),
                ])
            }
        })
        .collect()
}

fn overlay(app: &App, frame: &mut Frame) {
    if let Some(job) = app.job.as_ref().filter(|job| !job.hidden) {
        let width = 84;
        let inner = usize::from(width) - 6;
        let step = job.progress.borrow().clone();
        let seconds = job.since.elapsed().as_secs();
        let mut body = vec![
            title_line(&job.title, "esc hide", inner, false),
            blank(),
            Line::from(vec![
                span("◌ ", theme::lamp()),
                span(if step.is_empty() { "Starting".to_owned() } else { step }, theme::text()),
                span(format!("  · {seconds} s"), theme::dim()),
            ]),
        ];
        if let Some(hint) = &job.hint {
            body.push(blank());
            body.extend(wrapped(&format!("▲ {hint}"), inner, theme::secondary()));
        }
        body.push(blank());
        body.push(Line::from(span(
            "esc hides this and it keeps going in the background · q quits porchlight's screen, not the sharing",
            theme::faint(),
        )));
        let area = popup(frame, width, height_of(&body), false);
        fill(frame, area, &body);
        return;
    }

    match &app.mode {
        Mode::Normal => {}
        Mode::Busy(text) => {
            let area = popup(frame, u16::try_from(text.width() + 10).unwrap_or(60), 5, false);
            fill(frame, area, &[Line::from(vec![span("◌ ", theme::lamp()), span(text.clone(), theme::text())])]);
        }
        Mode::Message { title, lines, danger } => {
            let width = 84;
            let mut body = vec![title_line(title, "any key", usize::from(width) - 6, *danger), blank()];
            for line in lines {
                let style = if line.starts_with('✗') || line.starts_with('▲') {
                    theme::danger()
                } else if line.starts_with('✓') {
                    theme::state()
                } else {
                    theme::text()
                };
                body.extend(wrapped(line, usize::from(width) - 6, style));
            }
            let area = popup(frame, width, height_of(&body), *danger);
            fill(frame, area, &body);
        }
        Mode::Confirm { title, lines, typed, buffer, danger, .. } => {
            let width = 86;
            let inner = usize::from(width) - 6;
            let mut body = vec![title_line(title, "esc", inner, *danger), blank()];
            for line in lines {
                body.extend(wrapped(line, inner, if line.starts_with("   ") { theme::text() } else { theme::dim() }));
            }
            body.push(blank());
            match typed {
                Some(word) => body.push(Line::from(vec![
                    span("type ", theme::dim()),
                    span(word.clone(), theme::bold()),
                    span(" to confirm   ", theme::dim()),
                    span(buffer.clone(), theme::text()),
                    span("█", theme::text()),
                ])),
                None => body.push(Line::from(keys(&[("enter", "yes"), ("esc", "no")]))),
            }
            let area = popup(frame, width, height_of(&body), *danger);
            fill(frame, area, &body);
        }
        Mode::Input { title, context, prompt, buffer, error, .. } => {
            let width = 90;
            let inner = usize::from(width) - 6;
            let mut body = vec![title_line(title, "esc", inner, false), blank()];
            body.extend(context_lines(context));
            if !context.is_empty() {
                body.push(blank());
            }
            let field = body.len();
            body.push(Line::from(vec![
                span(fit(prompt, 16), theme::dim()),
                span(buffer.clone(), theme::text()),
                span("█", theme::text()),
            ]));
            if let Some(error) = error {
                body.push(blank());
                for (index, line) in wrapped(error, inner - 3, theme::text()).into_iter().enumerate() {
                    let mut spans = vec![span(if index == 0 { "▲  " } else { "   " }, theme::danger())];
                    spans.extend(line.spans);
                    body.push(Line::from(spans));
                }
            }
            body.push(blank());
            body.push(Line::from(keys(&[("enter", "next"), ("esc", "cancel")])));
            let area = popup(frame, width, height_of(&body), false);
            let row =
                Rect { x: area.x - 2, y: area.y + u16::try_from(field).unwrap_or(0), width: area.width + 4, height: 1 };
            theme::glow(frame.buffer_mut(), row);
            frame.buffer_mut().set_line(row.x, row.y, &Line::from(span("▌", theme::lamp())), 1);
            fill(frame, area, &body);
        }
        Mode::Choose { title, context, question, options, cursor, .. } => {
            let width = 90;
            let inner = usize::from(width) - 6;
            let mut body = vec![title_line(title, "esc", inner, false), blank()];
            body.extend(context_lines(context));
            if !context.is_empty() {
                body.push(blank());
            }
            body.push(Line::from(span(question.clone(), theme::bold())));
            body.push(blank());
            let first = body.len();
            for (index, option) in options.iter().enumerate() {
                body.push(Line::from(vec![
                    span(fit(&option.label, 38), if index == *cursor { theme::bold() } else { theme::text() }),
                    span(option.hint.clone(), theme::dim()),
                ]));
            }
            body.push(blank());
            body.push(Line::from(keys(&[("↑↓", "choose"), ("enter", "next")])));
            let area = popup(frame, width, height_of(&body), false);
            let row = Rect {
                x: area.x - 2,
                y: area.y + u16::try_from(first + cursor).unwrap_or(0),
                width: area.width + 4,
                height: 1,
            };
            theme::glow(frame.buffer_mut(), row);
            frame.buffer_mut().set_line(row.x, row.y, &Line::from(span("▌", theme::lamp())), 1);
            fill(frame, area, &body);
        }
        Mode::Checklist { title, lines, items, cursor, action, note, step } => {
            let width = 100;
            let inner = usize::from(width) - 6;
            let mut body = vec![title_line(title, "esc", inner, false)];
            body.extend(lines.iter().map(|line| {
                Line::from(span(line.clone(), if line.ends_with('?') { theme::bold() } else { theme::dim() }))
            }));
            body.push(blank());
            let first = body.len();
            let diff = changes(items);
            for (index, item) in items.iter().enumerate() {
                let (mark, style) = if item.locked {
                    ("[•]  ", theme::faint())
                } else if item.checked {
                    ("[x]  ", theme::lamp())
                } else {
                    ("[ ]  ", theme::dim())
                };
                let impact = match (item.checked, item.start) {
                    (true, false) => span("+ new here", theme::secondary()),
                    (false, true) => span("− clients lose it", theme::danger()),
                    _ => span("", theme::dim()),
                };
                body.push(Line::from(vec![
                    span(mark, style),
                    span(
                        fit(&item.label, 16),
                        if index == *cursor {
                            theme::bold()
                        } else if item.locked {
                            theme::dim()
                        } else {
                            theme::text()
                        },
                    ),
                    span(
                        fit(&item.detail, inner.saturating_sub(40)),
                        if item.detail.starts_with('▲') { theme::danger() } else { theme::dim() },
                    ),
                    impact,
                ]));
            }
            if items.is_empty() {
                body.push(Line::from(span("Nothing to pick yet.", theme::dim())));
            }
            if !diff.is_empty() {
                body.push(blank());
                body.push(Line::from(heading("what changes")));
                body.extend(diff.iter().cloned());
            }
            if let Some(note) = note {
                body.push(blank());
                body.extend(wrapped(note, inner, theme::faint()));
            }
            body.push(blank());
            let count = if diff.is_empty() { items.iter().filter(|item| item.checked).count() } else { diff.len() };
            let label = match (diff.is_empty(), count) {
                (true, 0) => action.clone(),
                (true, _) => format!("{action} ({count})"),
                (false, _) => format!("{action} {}", plural(count, "change")),
            };
            let mut pairs = vec![("space", "tick"), ("enter", label.as_str())];
            if matches!(step, super::Step::AppLinks { .. } | super::Step::CommandLinks { .. }) {
                pairs.push(("+", "new link"));
            }
            body.push(Line::from(keys(&pairs)));
            let area = popup(frame, width, height_of(&body), false);
            let row = Rect {
                x: area.x - 2,
                y: area.y + u16::try_from(first + cursor).unwrap_or(0),
                width: area.width + 4,
                height: 1,
            };
            if !items.is_empty() {
                theme::glow(frame.buffer_mut(), row);
                frame.buffer_mut().set_line(row.x, row.y, &Line::from(span("▌", theme::lamp())), 1);
            }
            fill(frame, area, &body);
        }
        Mode::Code { code, client, verified, until, copied } => {
            let left = (until - now_ms()).max(0) / 1000;
            let mut cells: Spans = Vec::new();
            for (index, character) in code.chars().enumerate() {
                if index == 4 {
                    cells.push(span("   ", theme::dim()));
                }
                cells.push(span(format!(" {character} "), theme::lit_block()));
                cells.push(span(" ", theme::dim()));
            }
            let width = 56;
            let inner = usize::from(width) - 6;
            let pad = inner.saturating_sub(width_of(&cells)) / 2;
            cells.insert(0, span(" ".repeat(pad), theme::dim()));
            let mut body = vec![
                title_line("Approve with a code", "esc", inner, false),
                Line::from(span(cut(&format!("Type it on the page {client} opened."), inner), theme::dim())),
                blank(),
                Line::from(cells),
                blank(),
            ];
            if !verified {
                body.push(Line::from(span(cut(&format!("▲ {client} isn't verified."), inner), theme::danger())));
                body.push(Line::from(span("  Only type it if you just added this connector.", theme::danger())));
                body.push(blank());
            }
            body.push(spread(
                vec![span(format!("expires in {}:{:02}", left / 60, left % 60), theme::dim())],
                if *copied { vec![span("✓ copied", theme::state())] } else { key_hint("y", "copy") },
                inner,
            ));
            let area = popup(frame, width, height_of(&body), false);
            fill(frame, area, &body);
        }
        Mode::Try(state) => {
            let width = 90;
            let inner = usize::from(width) - 6;
            let mut body = vec![
                spread(
                    vec![
                        span("Try ", theme::dim()),
                        span(state.tool.clone(), theme::bold()),
                        span(format!("  {}", state.app), theme::dim()),
                    ],
                    vec![span("esc", theme::dim())],
                    inner,
                ),
                Line::from(span(cut(&state.about, inner), theme::dim())),
                blank(),
            ];
            if state.changes {
                body.push(Line::from(span(
                    "▲ This changes things. It really runs, the same as when a client calls it.",
                    theme::secondary(),
                )));
                body.push(blank());
            }
            let first = body.len();
            for (index, (name, value)) in state.inputs.iter().enumerate() {
                let mut spans = vec![span(fit(name, 10), theme::dim()), span(value.clone(), theme::text())];
                if index == state.field {
                    spans.push(span("█", theme::text()));
                }
                body.push(Line::from(spans));
            }
            if state.inputs.is_empty() {
                body.push(Line::from(span("no inputs", theme::faint())));
            }
            body.push(blank());
            body.push(Line::from(keys(&[("enter", "run"), ("tab", "next input")])));
            if let Some((ok, output)) = &state.output {
                body.push(Line::from(span("─".repeat(inner), Style::new().fg(theme::RULE))));
                let mut output_lines = output.lines();
                let took = output_lines.next().unwrap_or_default();
                body.push(Line::from(vec![
                    span("output  ", theme::dim()),
                    span(if *ok { "ok" } else { "failed" }, if *ok { theme::state() } else { theme::danger() }),
                    span(format!(" · {took}"), theme::dim()),
                ]));
                body.extend(output_lines.take(14).map(|line| Line::from(span(cut(line, inner), theme::text()))));
            }
            let area = popup(frame, width, height_of(&body), false);
            if !state.inputs.is_empty() {
                let row = Rect {
                    x: area.x - 2,
                    y: area.y + u16::try_from(first + state.field).unwrap_or(0),
                    width: area.width + 4,
                    height: 1,
                };
                theme::glow(frame.buffer_mut(), row);
                frame.buffer_mut().set_line(row.x, row.y, &Line::from(span("▌", theme::lamp())), 1);
            }
            fill(frame, area, &body);
        }
        Mode::Help => {
            let here = context_keys(app);
            let everywhere: &[(&str, &str)] = &[
                ("tab 1–5", "switch tabs"),
                ("↑↓ j k", "move · g G top and bottom"),
                ("a c x", "approve, show a code, deny the waiting request"),
                ("q", "quit · porchlight keeps sharing"),
            ];
            let row = |(key, what): &(&str, &str)| {
                Line::from(vec![span(fit(key, 12), theme::bold()), span(what.to_string(), theme::dim())])
            };
            let mut body =
                vec![title_line("Keys", "any key", 60, false), blank(), Line::from(span("HERE", theme::dim()))];
            body.extend(here.iter().map(row));
            body.extend([blank(), Line::from(span("EVERYWHERE", theme::dim()))]);
            body.extend(everywhere.iter().map(row));
            let area = popup(frame, 66, height_of(&body), false);
            fill(frame, area, &body);
        }
    }
}
