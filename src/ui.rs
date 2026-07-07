//! Rendering, themed with the lilac palette ported from the user's nvim.
//! Layout: [ sidebar | main( tabline / transcript / composer / status ) ] with a
//! which-key popup overlay for pending leader chords, and a full cheatsheet
//! popup (Space z z).

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Chat, Entry, Focus, Mode, Pending};
use crate::theme as t;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn title_of(c: &Chat) -> String {
    if c.title.trim().is_empty() {
        "untitled".to_string()
    } else {
        c.title.clone()
    }
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let side_w = if app.sidebar_open {
        35u16.min(area.width.saturating_sub(24))
    } else {
        0
    };
    let cols = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).split(area);
    if app.sidebar_open && side_w > 0 {
        render_sidebar(f, cols[0], app);
    }
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(cols[1]);
    render_tabline(f, rows[0], app);
    render_transcript(f, rows[1], app);
    render_composer(f, rows[2], app);
    render_status(f, rows[3], app);

    if app.help_open {
        render_help(f, area);
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

    let order = app.visible_order();
    let mut lines: Vec<Line> = Vec::new();
    let mut last_qual: Option<Option<String>> = None;
    let mut idx_in_cat = 0usize;

    for &i in &order {
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

        let is_active = i == app.active;
        let editing = app.mode == Mode::Rename && is_active;
        let glyph = if c.in_flight {
            SPIN[app.spinner % SPIN.len()]
        } else {
            "●"
        };
        let digit = std::char::from_digit(idx_in_cat as u32, 10).unwrap_or(' ');
        let marker = if is_active { "▸" } else { " " };

        let mut style = Style::default().fg(if c.in_flight {
            t::AMBER
        } else if is_active {
            if focused {
                t::PINK
            } else {
                t::FG
            }
        } else {
            t::DIM
        });
        if is_active {
            style = style.add_modifier(Modifier::BOLD);
            if focused {
                style = style.bg(t::SELECTION);
            }
        }
        if editing {
            style = Style::default()
                .fg(t::PINK)
                .bg(t::SELECTION)
                .add_modifier(Modifier::BOLD);
        }

        let name = if editing {
            format!("{}▌", app.rename_buf)
        } else {
            title_of(c)
        };
        let label = if idx_in_cat < 10 {
            format!("{marker} {digit} {glyph} {name}")
        } else {
            format!("{marker}   {glyph} {name}")
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

fn render_tabline(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " aeovim ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(t::GUTTER)),
    ];
    for (n, &i) in app.visible_order().iter().enumerate() {
        let c = &app.chats[i];
        let glyph = if c.in_flight {
            SPIN[app.spinner % SPIN.len()]
        } else {
            "●"
        };
        let label = format!("{}:{} {glyph}  ", n + 1, title_of(c));
        let st = if i == app.active {
            Style::default().fg(t::FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t::DIM)
        };
        spans.push(Span::styled(label, st));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let idx = app.active;
    let spin = app.spinner;
    let lines = build_lines(&app.chats[idx], spin);
    let total = lines.len();
    let h = area.height as usize;
    let max_scroll = total.saturating_sub(h);
    app.chats[idx].last_max_scroll = max_scroll as u16;

    let chat = &app.chats[idx];
    let scroll = if chat.follow {
        max_scroll
    } else {
        (chat.scroll as usize).min(max_scroll)
    };
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        area,
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
        Mode::Normal => (t::BORDER, " prompt "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(title, Style::default().fg(t::DIM)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let content = match app.mode {
        Mode::Normal => Line::from(Span::styled(
            " i compose · Space leader · Space zz help · Ctrl-h/l panes · q quit",
            Style::default().fg(t::DIM),
        )),
        Mode::Rename => Line::from(Span::styled(
            " naming in sidebar — type a name · Enter confirm · Esc cancel",
            Style::default().fg(t::DIM),
        )),
        Mode::Insert => Line::from(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(accent)),
            Span::styled(app.input.clone(), Style::default().fg(t::FG)),
        ]),
        Mode::Command => Line::from(vec![
            Span::styled(": ".to_string(), Style::default().fg(accent)),
            Span::styled(app.cmd.clone(), Style::default().fg(t::FG)),
        ]),
    };
    f.render_widget(Paragraph::new(content), inner);

    // Real terminal cursor for the composer inputs; rename edits inline in the
    // sidebar instead (its own ▌).
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
    };
    let c = app.active_chat();
    let perm = if app.dangerous {
        "⚠ dangerous"
    } else {
        "acceptEdits"
    };
    let sid = &c.session_id[..8.min(c.session_id.len())];
    let mid = format!(
        "  {}  ·  {perm}  ·  ${:.4}  ·  {sid}  ",
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

fn render_whichkey(f: &mut Frame, area: Rect, pending: Pending) {
    let (title, entries): (&str, Vec<(&str, &str)>) = match pending {
        Pending::Leader => (
            "leader",
            vec![
                ("b", "toggle sidebar + focus"),
                ("z z", "help / all keybinds"),
                ("e", "+explorer"),
                ("s", "+split"),
                ("t", "+tab"),
                ("a", "add + name chat"),
                ("0-9", "jump to chat N"),
            ],
        ),
        Pending::LeaderE => (
            "leader e — explorer",
            vec![
                ("e", "toggle sidebar"),
                ("f", "focus sidebar"),
                ("c", "close sidebar"),
                ("r", "refresh / save"),
            ],
        ),
        Pending::LeaderS => (
            "leader s — split",
            vec![
                ("v", "vsplit (soon)"),
                ("h", "hsplit (soon)"),
                ("m", "zoom (soon)"),
                ("x", "close pane (soon)"),
            ],
        ),
        Pending::LeaderT => (
            "leader t — tab",
            vec![
                ("o", "new chat"),
                ("x", "close chat"),
                ("n", "next chat"),
                ("p", "prev chat"),
            ],
        ),
        Pending::LeaderZ => ("leader z", vec![("z", "help / all keybinds")]),
        Pending::G => (
            "g",
            vec![("g", "top"), ("t", "next chat"), ("T", "prev chat")],
        ),
        Pending::None => return,
    };

    let w = 34u16.min(area.width);
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
    // Sections of (key, description). "" key = section header.
    let rows: &[(&str, &str)] = &[
        ("", "MODES"),
        ("i", "compose a prompt (insert)"),
        ("Esc", "back to normal"),
        ("Enter", "send prompt / open chat"),
        (":", "command line (:new :loop :agent :q)"),
        ("", "PANES & FOCUS"),
        ("Ctrl-h / Ctrl-l", "focus sidebar / chat pane"),
        ("h / l", "focus sidebar / chat (at edges)"),
        ("Space b", "toggle sidebar + focus it"),
        ("Ctrl-e", "focus sidebar (quick menu)"),
        ("", "CHATS"),
        ("H / L", "prev / next chat"),
        ("Tab / S-Tab", "cycle chats"),
        ("Space 0-9", "jump to chat N in current group"),
        ("n", "new chat + compose"),
        ("", "SIDEBAR (when focused)"),
        ("j / k", "move down / up (live-switch)"),
        ("a", "add chat + name it inline"),
        ("r", "rename chat"),
        ("d", "close chat"),
        ("", "SCROLL"),
        ("j / k", "line down / up"),
        ("Ctrl-d / Ctrl-u", "half page"),
        ("gg / G", "top / bottom (follow)"),
        ("", "LEADER (Space)"),
        ("Space e e/f/c", "sidebar toggle / focus / close"),
        ("Space t o/x/n/p", "new / close / next / prev chat"),
        ("Space s ...", "splits (coming next)"),
        ("Space a", "add + name chat"),
        ("Space z z", "this help"),
        ("", "MISC"),
        ("q", "quit    ·  /loop <x>  tag chat as loop"),
    ];

    let w = 60u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
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
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner,
    );
}
