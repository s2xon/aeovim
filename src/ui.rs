//! Rendering, themed with the lilac palette from the user's nvim.
//! Layout: [ sidebar(spaces) | main( active-space panes / lualine status / prompt ) ].
//! The active space renders its 1–4 chats as split panes under a group header.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{
    chat_title, space_name, App, Chat, Entry, Focus, Mode, Pending, RenameTarget, SplitDir,
};
use crate::theme as t;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SEP_R: &str = "\u{e0b0}"; // powerline right-filled

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
        Constraint::Length(1), // lualine status
        Constraint::Length(3), // prompt (flush to bottom)
    ])
    .split(cols[1]);
    if app.spaces.is_empty() {
        render_empty(f, rows[0]);
        render_status(f, rows[1], app); // keep the mode indicator
        render_composer(f, rows[2], app); // and the command/prompt line (:q works)
        if app.help_open {
            render_help(f, area);
        }
        return;
    }
    render_active_space(f, rows[0], app);
    render_status(f, rows[1], app);
    render_composer(f, rows[2], app);

    if app.slash_active() {
        render_slash_popup(f, rows[2], app);
    }

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
            " spaces ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, sp) in app.spaces.iter().enumerate() {
        let is_active = i == app.active_space;
        let is_cursor = focused && i == app.sidebar_cursor;
        let is_selected = app.is_selected(sp.id);
        let editing =
            app.mode == Mode::Rename && is_cursor && app.rename_target == RenameTarget::Space;
        let inflight = sp.chats.iter().any(|c| c.in_flight);

        let glyph = if is_selected {
            "✓"
        } else if inflight {
            // running: flash filled ↔ hollow circle (no colour change)
            if (app.spinner / 4) % 2 == 0 {
                "●"
            } else {
                "○"
            }
        } else {
            "●"
        };
        let marker = if is_active {
            "▸"
        } else if is_cursor {
            "›"
        } else {
            " "
        };
        let fg = if is_selected {
            t::PINK
        } else if is_active {
            t::FG
        } else {
            t::DIM
        };
        let mut style = Style::default().fg(fg);
        if is_active || is_cursor || is_selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        if editing {
            style = Style::default().fg(t::PINK).add_modifier(Modifier::BOLD);
        }

        let name = if editing {
            format!("{}▌", app.rename_buf)
        } else {
            space_name(sp)
        };
        let count = if sp.chats.len() > 1 {
            format!(" ({})", sp.chats.len())
        } else {
            String::new()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {glyph} {name}{count}"),
            style,
        )));
    }
    if app.spaces.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no spaces",
            Style::default().fg(t::DIM),
        )));
        lines.push(Line::from(Span::styled(
            "  press n to start",
            Style::default().fg(t::PERI),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_empty(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::GUTTER));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mid = inner.height / 2;
    let lines: Vec<Line> = (0..inner.height)
        .map(|r| {
            if r == mid {
                Line::from(Span::styled(
                    "start a space  —  press  n",
                    Style::default().fg(t::PERI).add_modifier(Modifier::BOLD),
                ))
                .centered()
            } else if r == mid + 1 {
                Line::from(Span::styled(
                    "you have no open spaces",
                    Style::default().fg(t::DIM),
                ))
                .centered()
            } else {
                Line::from("")
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// Chats inside a space are split by a single thin divider — not boxed.
// Returns the pane rects plus divider rects (bool = horizontal line).
fn space_layout(inner: Rect, n: usize, dir: SplitDir) -> (Vec<Rect>, Vec<(Rect, bool)>) {
    use Constraint::{Length, Min};
    match n {
        0 | 1 => (vec![inner], Vec::new()),
        2 => match dir {
            SplitDir::V => {
                let p = Layout::horizontal([Min(1), Length(1), Min(1)]).split(inner);
                (vec![p[0], p[2]], vec![(p[1], false)])
            }
            SplitDir::H => {
                let p = Layout::vertical([Min(1), Length(1), Min(1)]).split(inner);
                (vec![p[0], p[2]], vec![(p[1], true)])
            }
        },
        3 => {
            let rows = Layout::vertical([Min(1), Length(1), Min(1)]).split(inner);
            let top = Layout::horizontal([Min(1), Length(1), Min(1)]).split(rows[0]);
            (
                vec![top[0], top[2], rows[2]],
                vec![(rows[1], true), (top[1], false)],
            )
        }
        _ => {
            let rows = Layout::vertical([Min(1), Length(1), Min(1)]).split(inner);
            let top = Layout::horizontal([Min(1), Length(1), Min(1)]).split(rows[0]);
            let bot = Layout::horizontal([Min(1), Length(1), Min(1)]).split(rows[2]);
            (
                vec![top[0], top[2], bot[0], bot[2]],
                vec![(rows[1], true), (top[1], false), (bot[1], false)],
            )
        }
    }
}

fn put_char(f: &mut Frame, x: u16, y: u16, ch: &str, st: Style) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(ch.to_string(), st))),
        Rect {
            x,
            y,
            width: 1,
            height: 1,
        },
    );
}

fn render_active_space(f: &mut Frame, region: Rect, app: &mut App) {
    let si = app.active_space;
    let (n, zoom, dir) = {
        let sp = &app.spaces[si];
        (sp.chats.len(), sp.zoom, sp.split_dir)
    };
    let title = {
        let sp = &app.spaces[si];
        let count = if n > 1 { format!(" ({n}) ") } else { " ".to_string() };
        Line::from(vec![
            Span::styled(
                format!(" {}", space_name(sp)),
                Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(count, Style::default().fg(t::DIM)),
        ])
    };
    let focused_main = app.focus == Focus::Main;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused_main { t::PURPLE } else { t::GUTTER }))
        .title(title);
    let inner = block.inner(region);
    f.render_widget(block, region);

    if zoom || n == 1 {
        let ci = app.spaces[si].fi();
        render_chat_body(f, inner, app, si, ci, true);
        return;
    }
    let (panes, dividers) = space_layout(inner, n, dir);
    let dstyle = Style::default().fg(t::GUTTER);
    for (drect, _) in dividers.iter().filter(|(_, h)| *h) {
        // horizontal divider — join the side borders with ├ … ┤
        let w = region.width as usize;
        let mut line = String::from("├");
        for _ in 0..w.saturating_sub(2) {
            line.push('─');
        }
        if w >= 2 {
            line.push('┤');
        }
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(line, dstyle))),
            Rect {
                x: region.x,
                y: drect.y,
                width: region.width,
                height: 1,
            },
        );
    }
    for (drect, _) in dividers.iter().filter(|(_, h)| !*h) {
        // vertical divider — join top/bottom with ┬ … ┴
        let lines: Vec<Line> = (0..drect.height)
            .map(|_| Line::from(Span::styled("│", dstyle)))
            .collect();
        f.render_widget(Paragraph::new(lines), *drect);
        put_char(f, drect.x, drect.y.saturating_sub(1), "┬", dstyle);
        put_char(f, drect.x, drect.y + drect.height, "┴", dstyle);
    }
    for ci in 0..n {
        if let Some(r) = panes.get(ci).copied() {
            render_chat_body(f, r, app, si, ci, true);
        }
    }
}

fn render_chat_body(f: &mut Frame, rect: Rect, app: &mut App, si: usize, ci: usize, header: bool) {
    let is_focus = app.focus == Focus::Main && ci == app.spaces[si].fi();
    let body = if header {
        let name = chat_title(&app.spaces[si].chats[ci]);
        let dot_col = if is_focus { t::PURPLE } else { t::DIM };
        let glyph = "●";
        let marker = if is_focus { "▸" } else { " " };
        let hdr = Line::from(vec![
            Span::styled(format!("{marker} {glyph} "), Style::default().fg(dot_col)),
            Span::styled(
                name,
                if is_focus {
                    Style::default().fg(t::FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t::DIM)
                },
            ),
        ]);
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(rect);
        f.render_widget(Paragraph::new(hdr), parts[0]);
        parts[1]
    } else {
        rect
    };

    let spin = app.spinner;
    let lines = build_lines(&app.spaces[si].chats[ci], spin);
    let total = lines.len();
    let h = body.height as usize;
    let max_scroll = total.saturating_sub(h);
    app.spaces[si].chats[ci].last_max_scroll = max_scroll as u16;
    let chat = &app.spaces[si].chats[ci];
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
            Entry::Tool(x) => {
                for (i, l) in x.split('\n').enumerate() {
                    let st = if i == 0 {
                        tool_st.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(t::DIM)
                    };
                    out.push(Line::from(Span::styled(format!("  {l}"), st)));
                }
            }
            Entry::ToolResult { ok, text } => {
                let col = if *ok { t::GUTTER } else { t::RED };
                for (i, l) in text.split('\n').enumerate() {
                    let prefix = if i == 0 { "  ⎿ " } else { "     " };
                    out.push(Line::from(Span::styled(
                        format!("{prefix}{l}"),
                        Style::default().fg(col),
                    )));
                }
            }
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
            let mut line = md_line("  ", l, body);
            if j + 1 == parts.len() {
                line.spans.push(Span::styled("▌".to_string(), body));
            }
            out.push(line);
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

fn md_flush(out: &mut Vec<Span<'static>>, buf: &mut String, base: Style) {
    if !buf.is_empty() {
        out.push(Span::styled(std::mem::take(buf), base));
    }
}

fn find_bold(s: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < s.len() {
        if s[i] == '*' && s[i + 1] == '*' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Small inline markdown: **bold**, *italic* / _italic_, `code`.
fn md_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let code_st = Style::default().fg(t::PERI);
    let mut i = 0;
    while i < n {
        if s[i] == '`' {
            if let Some(j) = (i + 1..n).find(|&k| s[k] == '`') {
                md_flush(&mut out, &mut buf, base);
                out.push(Span::styled(s[i + 1..j].iter().collect::<String>(), code_st));
                i = j + 1;
                continue;
            }
        }
        if s[i] == '*' && i + 1 < n && s[i + 1] == '*' {
            if let Some(j) = find_bold(&s, i + 2) {
                md_flush(&mut out, &mut buf, base);
                out.push(Span::styled(
                    s[i + 2..j].iter().collect::<String>(),
                    base.add_modifier(Modifier::BOLD),
                ));
                i = j + 2;
                continue;
            }
        }
        if s[i] == '*' || s[i] == '_' {
            let d = s[i];
            if let Some(j) = (i + 1..n).find(|&k| s[k] == d) {
                if j > i + 1 {
                    md_flush(&mut out, &mut buf, base);
                    out.push(Span::styled(
                        s[i + 1..j].iter().collect::<String>(),
                        base.add_modifier(Modifier::ITALIC),
                    ));
                    i = j + 1;
                    continue;
                }
            }
        }
        buf.push(s[i]);
        i += 1;
    }
    md_flush(&mut out, &mut buf, base);
    out
}

/// A transcript body line: indent prefix + markdown (headers rendered bold).
fn md_line(prefix: &str, text: &str, base: Style) -> Line<'static> {
    let cleaned = crate::app::clean_line(text);
    let text = cleaned.as_str();
    let trimmed = text.trim_start();
    let header = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "));
    if let Some(rest) = header {
        return Line::from(vec![
            Span::styled(prefix.to_string(), base),
            Span::styled(
                rest.to_string(),
                Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    // bullet lists
    let bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "));
    if let Some(rest) = bullet {
        let mut spans = vec![Span::styled(format!("{prefix}• "), Style::default().fg(t::PERI))];
        spans.extend(md_spans(rest, base));
        return Line::from(spans);
    }
    // blockquote
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return Line::from(vec![
            Span::styled(format!("{prefix}▏ "), Style::default().fg(t::GUTTER)),
            Span::styled(rest.to_string(), Style::default().fg(t::DIM).add_modifier(Modifier::ITALIC)),
        ]);
    }
    let mut spans = vec![Span::styled(prefix.to_string(), base)];
    spans.extend(md_spans(text, base));
    Line::from(spans)
}

fn render_slash_popup(f: &mut Frame, composer: Rect, app: &App) {
    let matches = app.slash_matches();
    let shown = matches.len().clamp(1, 8) as u16;
    let w = 40u16.min(composer.width);
    let h = shown + 2;
    let y = composer.y.saturating_sub(h);
    let rect = Rect {
        x: composer.x,
        y,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::BORDER))
        .title(Span::styled(
            " commands ",
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let win = inner.height as usize;
    let start = if app.slash_sel >= win {
        app.slash_sel + 1 - win
    } else {
        0
    };
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(win)
        .map(|(i, cmd)| {
            let selected = i == app.slash_sel;
            let caret = if selected { "› " } else { "  " };
            let st = if selected {
                Style::default().fg(t::PINK).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::FG)
            };
            Line::from(Span::styled(format!("{caret}/{cmd}"), st))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn push_block(out: &mut Vec<Line<'static>>, label: &str, lbl: Style, text: &str, body: Style) {
    out.push(Line::from(Span::styled(label.to_string(), lbl)));
    let mut in_fence = false;
    for l in text.split('\n') {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(Line::from(Span::styled(
                "  ┄┄┄┄".to_string(),
                Style::default().fg(t::GUTTER),
            )));
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {}", crate::app::clean_line(l)),
                Style::default().fg(t::PERI),
            )));
        } else {
            out.push(md_line("  ", l, body));
        }
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
        Mode::Rename if app.rename_target == RenameTarget::Chat => Line::from(vec![
            Span::styled("rename ❯ ".to_string(), Style::default().fg(accent)),
            Span::styled(app.rename_buf.clone(), Style::default().fg(t::FG)),
        ]),
        Mode::Rename => Line::from(Span::styled(
            " renaming space in sidebar — Enter confirm · Esc cancel",
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
        Mode::Rename if app.rename_target == RenameTarget::Chat => {
            Some(9 + app.rename_buf.chars().count())
        }
        _ => None,
    };
    if let Some(col) = cursor_col {
        let x = (inner.x + col as u16).min(inner.x + inner.width.saturating_sub(1));
        f.set_cursor_position(Position::new(x, inner.y));
    }
}

// lualine-style powerline statusline (no cost).
fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, mode_col) = match app.mode {
        Mode::Normal => ("NORMAL", t::MODE_NORMAL),
        Mode::Insert => ("INSERT", t::MODE_INSERT),
        Mode::Command => ("COMMAND", t::MODE_COMMAND),
        Mode::Rename => ("RENAME", t::MODE_VISUAL),
        Mode::Picker => ("FIND", t::MODE_VISUAL),
        Mode::Confirm => ("CONFIRM", t::RED),
    };
    if app.spaces.is_empty() {
        let line = Line::from(vec![
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(t::PANEL)
                    .bg(mode_col)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  no spaces — press n to start ",
                Style::default().fg(t::DIM),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let sp = &app.spaces[app.active_space];
    let c = app.cur_chat();
    let where_ = if sp.chats.len() > 1 {
        format!(" {} · {} ", space_name(sp), chat_title(c))
    } else {
        format!(" {} ", space_name(sp))
    };
    let perm = if app.dangerous {
        "⚠ dangerous"
    } else {
        "acceptEdits"
    };
    let info = format!(" {} · {perm} ", app.model_display);

    let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(12)]).split(area);

    // left: [ mode ][ where ][ info ]
    let left = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(t::PANEL)
                .bg(mode_col)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(SEP_R, Style::default().fg(mode_col).bg(t::SELECTION)),
        Span::styled(where_, Style::default().fg(t::FG).bg(t::SELECTION)),
        Span::styled(SEP_R, Style::default().fg(t::SELECTION).bg(t::PANEL)),
        Span::styled(info, Style::default().fg(t::DIM).bg(t::PANEL)),
        Span::styled(SEP_R, Style::default().fg(t::PANEL)),
    ]);
    f.render_widget(Paragraph::new(left), cols[0]);

    // right: activity — flashing dot, not a spinner (the one spinner is in the body)
    let right = if c.in_flight {
        Line::from(vec![
            Span::styled("● ", Style::default().fg(t::AMBER)),
            Span::styled("working ", Style::default().fg(t::DIM)),
        ])
    } else {
        Line::from(Span::styled("idle ", Style::default().fg(t::DIM)))
    };
    f.render_widget(Paragraph::new(right).right_aligned(), cols[1]);
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
            " merge space ",
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
            "  type a space to merge — or a new chat name".to_string()
        } else {
            format!("  ↵ new chat \"{q}\" in this space")
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(t::PERI))));
    } else {
        for (row, &si) in cands.iter().enumerate().skip(start).take(win) {
            let sp = &app.spaces[si];
            let selected = row == app.picker_sel;
            let caret = if selected { "› " } else { "  " };
            let count = if sp.chats.len() > 1 {
                format!(" ({})", sp.chats.len())
            } else {
                String::new()
            };
            let st = if selected {
                Style::default().fg(t::PINK).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t::FG)
            };
            lines.push(Line::from(Span::styled(
                format!("{caret}{}{count}", space_name(sp)),
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
                ("e", "+explorer (sidebar)"),
                ("z z", "help / all keybinds"),
                ("n", "+new (chat / space)"),
                ("s", "+space (panes)"),
                ("t", "+tab"),
                ("a", "new space + name"),
                ("1-0", "focus space N"),
            ],
        ),
        Pending::LeaderE => (
            "leader e — explorer",
            vec![
                ("e", "toggle sidebar"),
                ("f", "focus sidebar"),
                ("c", "close sidebar"),
            ],
        ),
        Pending::LeaderN => (
            "leader n — new",
            vec![("c", "new chat (tab in space)"), ("s", "new space")],
        ),
        Pending::LeaderS => (
            "leader s — space",
            vec![
                ("c", "merge a space in / new chat"),
                ("n", "add a chat pane"),
                ("p", "pop chat → new space"),
                ("x", "close pane"),
                ("v / h", "split vertical / horizontal"),
                ("m", "zoom pane"),
            ],
        ),
        Pending::LeaderT => (
            "leader t",
            vec![
                ("n", "new chat (tab) in space"),
                ("o", "new space"),
                ("x", "close pane"),
            ],
        ),
        Pending::LeaderZ => ("leader z", vec![("z", "help / all keybinds")]),
        Pending::G => (
            "g",
            vec![("g", "top"), ("t", "next pane"), ("T", "prev pane")],
        ),
        Pending::None => return,
    };

    let w = 40u16.min(area.width);
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
                    format!(" {k:>5} "),
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
        ("Enter", "send · activate space (sidebar)"),
        (":", "command (:new :pop :vsplit :q)"),
        ("", "SPACES (sidebar)"),
        ("j / k", "move  ·  { / }  jump 5"),
        ("Enter", "activate space (show it)"),
        ("Space 1-0", "focus space N"),
        ("a", "new space + name it"),
        ("r", "rename space"),
        ("s", "select / deselect (multi)"),
        ("m", "merge selected spaces (≤4 chats)"),
        ("d", "delete space(s) — confirm"),
        ("n", "new space + compose"),
        ("", "PANES (inside a space)"),
        ("Ctrl-h/j/k/l", "focus pane / sidebar"),
        ("Tab / H / L", "cycle panes"),
        ("Space s c", "merge a space in / type new chat"),
        ("Space s n", "add a chat pane"),
        ("Space s p", "pop pane → its own space"),
        ("Space s x", "close pane (last one deletes space)"),
        ("Space s v/h/m", "split V / H / zoom"),
        ("", "SCROLL"),
        ("j / k", "line  ·  Ctrl-d/u half  ·  gg/G top/bottom"),
        ("", "MISC"),
        ("Space e", "toggle sidebar  ·  Space zz  help  ·  q quit"),
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
                        format!(" {k:>14}  "),
                        Style::default().fg(t::PINK).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(d.to_string(), Style::default().fg(t::FG)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
