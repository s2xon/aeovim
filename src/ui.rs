//! Rendering, themed with the lilac palette from the user's nvim.
//! Layout: [ sidebar | main( spaces / composer / status ) ]. The main area holds
//! one or more *spaces* (panes); each space shows a `● N` focus badge, a tab bar
//! of its inter-tabs over a thin rule, and the focused tab's chat. Overlays:
//! help (Space zz), telescope-style picker, confirm dialog, which-key popup.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Chat, Entry, Focus, Mode, Pending, SplitDir};
use crate::theme as t;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn title_of(c: &Chat) -> String {
    if c.title.trim().is_empty() {
        "untitled".to_string()
    } else {
        c.title.clone()
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let side_w = if app.sidebar_open {
        28u16.min(area.width.saturating_sub(20))
    } else {
        0
    };
    let cols = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).split(area);
    if app.sidebar_open && side_w > 0 {
        render_sidebar(f, cols[0], app);
    }
    let rows = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(cols[1]);
    render_spaces(f, rows[0], app);
    render_composer(f, rows[1], app);
    render_status(f, rows[2], app);

    if app.help_open {
        render_help(f, area);
    } else if app.mode == Mode::Picker {
        render_picker(f, area, app);
    } else if app.mode == Mode::Confirm {
        render_confirm(f, area, app);
    } else if app.pending != Pending::None {
        render_whichkey(f, area, app.pending);
    }
}

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Sidebar;
    let border = if focused { t::PURPLE } else { t::BORDER };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            " agents ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width as usize;

    let cur = app.cur_id();
    let order = app.visible_order();
    let mut lines: Vec<Line> = Vec::new();
    let mut last_qual: Option<Option<String>> = None;
    let mut idx_in_cat = 0usize;

    for (pos, &i) in order.iter().enumerate() {
        let c = &app.chats[i];
        let tq = c.qualifier.clone();
        if last_qual.as_ref() != Some(&tq) {
            if let Some(q) = &tq {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(thin_rule(q, width));
            }
            last_qual = Some(tq.clone());
            idx_in_cat = 0;
        }

        let is_cursor = focused && pos == app.sidebar_cursor;
        let is_current = c.id == cur;
        let is_open = app.is_open(c.id);
        let is_selected = app.is_selected(c.id);
        let editing = app.mode == Mode::Rename && is_cursor;

        let glyph = if is_selected {
            "✓"
        } else if c.in_flight {
            SPIN[app.spinner % SPIN.len()]
        } else {
            "●"
        };
        let digit = std::char::from_digit(idx_in_cat as u32, 10).unwrap_or(' ');
        let marker = if is_current {
            "▸"
        } else if is_cursor {
            "›"
        } else {
            " "
        };
        // active / open / selected chats are simply brighter — no background.
        let fg = if is_selected {
            t::PINK
        } else if c.in_flight {
            t::AMBER
        } else if is_current || is_open {
            t::FG
        } else {
            t::DIM
        };
        let mut style = Style::default().fg(fg);
        if is_current || is_cursor || is_selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        if editing {
            style = Style::default().fg(t::PINK).add_modifier(Modifier::BOLD);
        }

        let name = if editing {
            format!("{}▌", app.rename_buf)
        } else {
            title_of(c)
        };
        let label = if idx_in_cat < 10 {
            format!("{marker}{digit} {glyph} {name}")
        } else {
            format!("{marker}  {glyph} {name}")
        };
        lines.push(Line::from(Span::styled(label, style)));
        idx_in_cat += 1;
    }
    if order.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no agents",
            Style::default().fg(t::DIM),
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn thin_rule(name: &str, width: usize) -> Line<'static> {
    let used = 2 + name.chars().count() + 1;
    let dashes = width.saturating_sub(used);
    Line::from(vec![
        Span::styled("─ ".to_string(), Style::default().fg(t::GUTTER)),
        Span::styled(
            name.to_string(),
            Style::default().fg(t::PERI).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", "─".repeat(dashes)),
            Style::default().fg(t::GUTTER),
        ),
    ])
}

fn space_rects(area: Rect, n: usize, dir: SplitDir) -> Vec<Rect> {
    use Constraint::Percentage as P;
    match n {
        0 | 1 => vec![area],
        2 => match dir {
            SplitDir::V => Layout::horizontal([P(50), P(50)]).spacing(1).split(area).to_vec(),
            SplitDir::H => Layout::vertical([P(50), P(50)]).spacing(1).split(area).to_vec(),
        },
        3 => {
            let rows = Layout::vertical([P(50), P(50)]).spacing(1).split(area);
            let top = Layout::horizontal([P(50), P(50)]).spacing(1).split(rows[0]);
            vec![top[0], top[1], rows[1]]
        }
        _ => {
            let rows = Layout::vertical([P(50), P(50)]).spacing(1).split(area);
            let top = Layout::horizontal([P(50), P(50)]).spacing(1).split(rows[0]);
            let bot = Layout::horizontal([P(50), P(50)]).spacing(1).split(rows[1]);
            vec![top[0], top[1], bot[0], bot[1]]
        }
    }
}

fn render_spaces(f: &mut Frame, region: Rect, app: &mut App) {
    // even margins around the whole spaces region
    let region = Rect {
        x: region.x + 1,
        y: region.y,
        width: region.width.saturating_sub(2),
        height: region.height,
    };
    if app.zoom {
        let idx = app.active_pane;
        render_space(f, region, app, idx);
        return;
    }
    let rects = space_rects(region, app.panes.len(), app.split_dir);
    for i in 0..app.panes.len() {
        if let Some(r) = rects.get(i).copied() {
            render_space(f, r, app, i);
        }
    }
}

fn render_space(f: &mut Frame, rect: Rect, app: &mut App, i: usize) {
    let focused = app.focus == Focus::Main && i == app.active_pane;
    let (tabs, active_tab) = {
        let p = &app.panes[i];
        (p.tabs.clone(), p.active_tab)
    };
    let at = active_tab.min(tabs.len().saturating_sub(1));
    let cid = tabs[at];

    // border + focus badge (● N when focused, ○ N otherwise)
    let border_col = if focused { t::PURPLE } else { t::GUTTER };
    let (dot, dot_style) = if focused {
        (
            "●",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        )
    } else {
        ("○", Style::default().fg(t::DIM))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_col))
        .title(Span::styled(format!(" {dot} {} ", i + 1), dot_style));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let tabbar = parts[0];
    let sep = parts[1];
    let body = parts[2];

    // tab bar — the space's chats as tabs
    let mut spans: Vec<Span> = Vec::new();
    for (ti, &tid) in tabs.iter().enumerate() {
        let (name, inflight) = app
            .chats
            .iter()
            .find(|c| c.id == tid)
            .map(|c| (title_of(c), c.in_flight))
            .unwrap_or(("?".to_string(), false));
        let glyph = if inflight {
            SPIN[app.spinner % SPIN.len()]
        } else {
            "●"
        };
        let st = if ti == at {
            Style::default().fg(t::FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t::DIM)
        };
        if ti > 0 {
            spans.push(Span::styled("   ", Style::default().fg(t::GUTTER)));
        }
        spans.push(Span::styled(format!(" {glyph} {name}"), st));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), tabbar);

    // super-thin separator
    let rule = "─".repeat(sep.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(rule, Style::default().fg(t::GUTTER)))),
        sep,
    );

    // transcript of the focused tab's chat
    let cidx = app.chats.iter().position(|c| c.id == cid).unwrap_or(0);
    let spin = app.spinner;
    let lines = build_lines(&app.chats[cidx], spin);
    let total = lines.len();
    let h = body.height as usize;
    let max_scroll = total.saturating_sub(h);
    app.chats[cidx].last_max_scroll = max_scroll as u16;
    let chat = &app.chats[cidx];
    let scroll = if chat.follow {
        max_scroll
    } else {
        (chat.scroll as usize).min(max_scroll)
    };
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        body,
    );
}

fn build_lines(chat: &Chat, spin: usize) -> Vec<Line<'static>> {
    let user_lbl = Style::default().fg(t::PERI).add_modifier(Modifier::BOLD);
    let asst_lbl = Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD);
    let tool_st = Style::default().fg(t::AMBER);
    let note_st = Style::default().fg(t::DIM);
    let err_st = Style::default().fg(t::RED).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(t::FG);

    let mut out: Vec<Line> = Vec::new();
    for (i, e) in chat.transcript.iter().enumerate() {
        if i > 0 {
            out.push(Line::from(""));
        }
        match e {
            Entry::User(x) => push_block(&mut out, "you", user_lbl, x, body),
            Entry::Assistant(x) => push_block(&mut out, "claude", asst_lbl, x, body),
            Entry::Tool(n) => out.push(Line::from(Span::styled(format!("  ⚙ {n}"), tool_st))),
            Entry::Note(x) => out.push(Line::from(Span::styled(format!("  {x}"), note_st))),
            Entry::Error(x) => push_block(&mut out, "! error", err_st, x, err_st),
        }
    }

    if let Some(s) = &chat.streaming {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled("claude", asst_lbl)));
        let parts: Vec<&str> = s.split('\n').collect();
        for (j, l) in parts.iter().enumerate() {
            let mut content = format!("  {l}");
            if j + 1 == parts.len() {
                content.push('▌');
            }
            out.push(Line::from(Span::styled(content, body)));
        }
    } else if chat.in_flight {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled("claude", asst_lbl)));
        out.push(Line::from(Span::styled(
            format!("  {} …", SPIN[spin % SPIN.len()]),
            note_st,
        )));
    }
    out
}

fn push_block(out: &mut Vec<Line<'static>>, label: &str, lbl: Style, text: &str, body: Style) {
    out.push(Line::from(Span::styled(label.to_string(), lbl)));
    for l in text.split('\n') {
        out.push(Line::from(Span::styled(format!("  {l}"), body)));
    }
}

fn render_composer(f: &mut Frame, area: Rect, app: &App) {
    let (accent, title) = match app.mode {
        Mode::Insert => (t::PINK, " prompt "),
        Mode::Rename => (t::PURPLE, " rename "),
        Mode::Command => (t::AMBER, " command "),
        _ => (t::BORDER, " prompt "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(title, Style::default().fg(t::DIM)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let content = match app.mode {
        Mode::Insert => Line::from(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(accent)),
            Span::styled(app.input.clone(), Style::default().fg(t::FG)),
        ]),
        Mode::Command => Line::from(vec![
            Span::styled(": ".to_string(), Style::default().fg(accent)),
            Span::styled(app.cmd.clone(), Style::default().fg(t::FG)),
        ]),
        Mode::Rename => Line::from(Span::styled(
            " naming in sidebar — type a name · Enter confirm · Esc cancel",
            Style::default().fg(t::DIM),
        )),
        _ => Line::from(Span::styled(
            " i compose · Space leader · Space zz help · Ctrl-h/l panes · q quit",
            Style::default().fg(t::DIM),
        )),
    };
    f.render_widget(Paragraph::new(content), inner);

    let cursor_col = match app.mode {
        Mode::Insert => Some(2 + app.input.chars().count()),
        Mode::Command => Some(2 + app.cmd.chars().count()),
        _ => None,
    };
    if let Some(col) = cursor_col {
        let x = (inner.x + col as u16).min(inner.x + inner.width.saturating_sub(1));
        f.set_cursor_position(Position::new(x, inner.y));
    }
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match app.mode {
        Mode::Normal => ("NORMAL", t::MODE_NORMAL),
        Mode::Insert => ("INSERT", t::MODE_INSERT),
        Mode::Command => ("COMMAND", t::MODE_COMMAND),
        Mode::Rename => ("RENAME", t::MODE_VISUAL),
        Mode::Picker => ("FIND", t::MODE_VISUAL),
        Mode::Confirm => ("CONFIRM", t::RED),
    };
    let c = app.cur_chat();
    let perm = if app.dangerous {
        "⚠ dangerous"
    } else {
        "acceptEdits"
    };
    let name = title_of(c);
    let sid = &c.session_id[..8.min(c.session_id.len())];
    let mid = format!(
        "  {name}  ·  {}  ·  {perm}  ·  ${:.4}  ·  {sid}  ",
        app.model_display, c.cost
    );
    let activity = if c.in_flight {
        format!("{} working", SPIN[app.spinner % SPIN.len()])
    } else {
        "idle".to_string()
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(t::PANEL)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mid, Style::default().fg(t::DIM)),
        Span::styled(
            activity,
            Style::default().fg(if c.in_flight { t::AMBER } else { t::DIM }),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    let cands = app.picker_candidates();
    let q = app.picker_query.trim();
    let vis = cands.len().clamp(1, 12);
    let w = 54u16.min(area.width.saturating_sub(4));
    let h = ((vis as u16) + 3).min(area.height.saturating_sub(2)).max(4);
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::BORDER))
        .title(Span::styled(
            " find chat ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let results_area = parts[0];
    let prompt_area = parts[1];

    let win = results_area.height as usize;
    let start = if app.picker_sel >= win {
        app.picker_sel + 1 - win
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    if cands.is_empty() {
        let hint = if q.is_empty() {
            "  type to find — or a new name to create".to_string()
        } else {
            format!("  ↵ create \"{q}\"")
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(t::PERI))));
    } else {
        for (row, &ci) in cands.iter().enumerate().skip(start).take(win) {
            let c = &app.chats[ci];
            let selected = row == app.picker_sel;
            let caret = if selected { "› " } else { "  " };
            let st = if selected {
                Style::default().fg(t::PINK).add_modifier(Modifier::BOLD)
            } else if c.in_flight {
                Style::default().fg(t::AMBER)
            } else {
                Style::default().fg(t::FG)
            };
            lines.push(Line::from(Span::styled(
                format!("{caret}{}", title_of(c)),
                st,
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), results_area);

    let prompt = Line::from(vec![
        Span::styled("❯ ", Style::default().fg(t::PINK).add_modifier(Modifier::BOLD)),
        Span::styled(app.picker_query.clone(), Style::default().fg(t::FG)),
    ]);
    f.render_widget(Paragraph::new(prompt), prompt_area);
    let x = (prompt_area.x + 2 + app.picker_query.chars().count() as u16)
        .min(prompt_area.x + prompt_area.width.saturating_sub(1));
    f.set_cursor_position(Position::new(x, prompt_area.y));
}

fn render_confirm(f: &mut Frame, area: Rect, app: &App) {
    let msg = app.confirm_msg.clone();
    let w = (msg.chars().count() as u16 + 6).clamp(24, area.width.saturating_sub(4));
    let rect = centered(area, w, 3);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::RED))
        .title(Span::styled(
            " confirm ",
            Style::default().fg(t::RED).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(t::FG),
        ))),
        inner,
    );
}

fn render_whichkey(f: &mut Frame, area: Rect, pending: Pending) {
    let (title, entries): (&str, Vec<(&str, &str)>) = match pending {
        Pending::Leader => (
            "leader",
            vec![
                ("e", "toggle sidebar"),
                ("z z", "help / all keybinds"),
                ("s", "+spaces (split / find)"),
                ("t", "+tab"),
                ("a", "add + name chat"),
                ("1-0", "focus space N"),
            ],
        ),
        Pending::LeaderS => (
            "leader s — spaces",
            vec![
                ("v", "vsplit + find/new chat"),
                ("h", "hsplit + find/new chat"),
                ("c", "combine chat (find / ＋new)"),
                ("d", "separate tab → new space"),
                ("x", "close space"),
                ("o", "only this space"),
                ("m", "zoom toggle"),
            ],
        ),
        Pending::LeaderT => (
            "leader t — tab",
            vec![
                ("o", "new chat"),
                ("x", "close chat"),
                ("n", "next tab"),
                ("p", "prev tab"),
            ],
        ),
        Pending::LeaderZ => ("leader z", vec![("z", "help / all keybinds")]),
        Pending::G => (
            "g",
            vec![("g", "top"), ("t", "next tab"), ("T", "prev tab")],
        ),
        Pending::None => return,
    };

    let w = 38u16.min(area.width);
    let h = (entries.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y + area.height.saturating_sub(h + 1),
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:>3} "),
                    Style::default().fg(t::PINK).add_modifier(Modifier::BOLD),
                ),
                Span::styled("→ ", Style::default().fg(t::GUTTER)),
                Span::styled(d.to_string(), Style::default().fg(t::FG)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("", "MODES"),
        ("i / Esc", "compose / normal"),
        ("Enter", "send · open chat in space"),
        (":", "command (:new :loop :vsplit :q)"),
        ("", "FOCUS & SPACES"),
        ("Ctrl-h/j/k/l", "focus space left/down/up/right (↔ sidebar)"),
        ("Space e", "toggle sidebar"),
        ("Space s v / s h", "split space + find/new chat"),
        ("Space s c", "find a chat (or ＋ new) → tab in this space"),
        ("Space s d", "separate current tab into a new space"),
        ("Space s x / s o", "close space / keep only this space"),
        ("Space s m", "zoom the focused space"),
        ("Tab / S-Tab", "cycle tabs in the focused space"),
        ("H / L", "prev / next tab"),
        ("", "CHATS"),
        ("j / k", "sidebar: move  ·  main: scroll"),
        ("{ / }", "jump group up / down (sidebar)"),
        ("Enter / l", "open selected chat in the focused space"),
        ("n", "new chat — new tab in this space"),
        ("a", "new chat + name it (in sidebar)"),
        ("r", "rename chat"),
        ("s", "select / deselect (multi-select)"),
        ("d", "delete chat(s) — asks to confirm"),
        ("Space 1-0", "focus space N (pane)"),
        ("", "SCROLL"),
        ("Ctrl-d / Ctrl-u", "half page  ·  gg / G  top / bottom"),
        ("", "MISC"),
        ("Space z z", "this help   ·   q  quit   ·   /loop  tag as loop"),
    ];

    let w = 64u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::PURPLE))
        .title(Span::styled(
            " aeovim — keybinds  (any key to close) ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            if k.is_empty() {
                Line::from(Span::styled(
                    format!(" {d}"),
                    Style::default().fg(t::PERI).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {k:>16}  "),
                        Style::default().fg(t::PINK).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(d.to_string(), Style::default().fg(t::FG)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
