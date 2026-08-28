//! Rendering — the redesigned multi-session surface, colour-token themed.
//!
//! Layout, per the design canvas: header row · thin rule · [ SESSIONS sidebar |
//! transcript ] · powerline statusline · composer. Overlays: ga fuzzy picker,
//! :tasks board, help, confirm. The sidebar is the pane that came back: one row
//! per session with a status glyph (●/○ running flash, ✗ error, ✓ idle).
//!
//! Perf model (the CodeWhale lesson): the settled transcript is pre-wrapped
//! into visual rows ONCE per change (`RenderCache`, keyed by rev/width/expand)
//! and each frame only clones the ≤height rows in view. The streaming tail is
//! tiny and rebuilt per frame.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{space_name, App, Chat, ChatStatus, DiffKind, Entry, Focus, Mode, Overlay};
use crate::theme;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SEP_R: &str = "\u{e0b0}"; // powerline right-filled
const SIDEBAR_W: u16 = 30;

/// Pre-wrapped visual rows of the settled transcript. Rebuilt only when the
/// transcript revision, pane width, or tool-expansion toggle changes.
pub struct RenderCache {
    rev: u64,
    width: u16,
    expand: bool,
    rows: Vec<Line<'static>>,
}

impl Default for RenderCache {
    fn default() -> Self {
        RenderCache {
            rev: u64::MAX, // never matches a real rev → first render builds
            width: 0,
            expand: false,
            rows: Vec::new(),
        }
    }
}

/// Compact elapsed: "8s" under a minute, "1m05s" past it.
fn fmt_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
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

/// Truncate to a display width (not a char count — CJK/emoji are 2 cells).
fn truncate_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Pad or truncate to exactly `w` display columns (board/picker columns).
fn cell(s: &str, w: usize) -> String {
    let t = truncate_width(s, w);
    let pad = w.saturating_sub(t.width());
    format!("{t}{}", " ".repeat(pad))
}

/// The ●/○ running flash from the old sidebar — no colour change, just shape.
fn flash_glyph(spinner: usize) -> &'static str {
    if (spinner / 4) % 2 == 0 {
        "●"
    } else {
        "○"
    }
}

/// The working-line loader: five phase-shifted bars from cli-spinners'
/// growVertical family — a rolling equalizer wave, coloured info → accent →
/// accent2 across the bars.
fn wave_spans(spin: usize) -> Vec<Span<'static>> {
    const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    const TABLE: [usize; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 6, 5, 4, 3, 2, 1];
    let th = theme::get();
    let cols = [th.info, th.accent, th.accent2, th.accent, th.info];
    (0..5)
        .map(|i| {
            let lvl = TABLE[(spin + i * 2) % TABLE.len()];
            Span::styled(BARS[lvl].to_string(), Style::default().fg(cols[i]))
        })
        .collect()
}

/// Status → (glyph, colour, meta word). Codex-simple: no cost on screen —
/// that lives behind `:cost`.
fn status_glyph(status: ChatStatus, running_secs: Option<u64>, spinner: usize) -> (&'static str, Color, String) {
    let th = theme::get();
    match status {
        ChatStatus::Running => (
            flash_glyph(spinner),
            th.accent2,
            fmt_secs(running_secs.unwrap_or(0)),
        ),
        ChatStatus::Error => ("✗", th.err, "error".into()),
        ChatStatus::Idle => ("✓", th.ok, String::new()),
        ChatStatus::New => ("○", th.dim, String::new()),
    }
}

/// Longest-running chat's elapsed seconds, for a space's sidebar meta.
fn space_running_secs(sp: &crate::app::Space) -> Option<u64> {
    sp.chats
        .iter()
        .filter(|c| c.in_flight)
        .filter_map(|c| c.turn_started.map(|t| t.elapsed().as_secs()))
        .max()
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let composer_h = composer_height(app, area.width.saturating_sub(4), area.height);
    let rows = Layout::vertical([
        Constraint::Length(1),          // header
        Constraint::Length(1),          // rule
        Constraint::Min(3),             // body: [sidebar | transcript]
        Constraint::Length(1),          // lualine status
        Constraint::Length(composer_h), // prompt (flush to bottom, auto-grows)
    ])
    .split(area);

    render_header(f, rows[0], app);
    render_rule(f, rows[1]);

    let side_w = if app.sidebar_open {
        SIDEBAR_W.min(area.width.saturating_sub(24))
    } else {
        0
    };
    let cols =
        Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).split(rows[2]);
    if side_w > 0 {
        render_sidebar(f, cols[0], app);
    }
    render_main(f, cols[1], app);

    render_status(f, rows[3], app);
    render_composer(f, rows[4], app);

    if app.slash_active() {
        render_slash_popup(f, rows[4], app);
    }

    match app.overlay {
        Overlay::Help => render_help(f, area),
        Overlay::Picker => render_picker(f, area, app),
        Overlay::DirPicker => render_dir_picker(f, area, app),
        Overlay::Board => render_board(f, area, app),
        Overlay::DiffPad => render_diffpad(f, area, app),
        Overlay::None => {
            if app.mode == Mode::Confirm {
                render_confirm(f, area, app);
            }
        }
    }
}

// Codex-simple header: name on the left, model dim on the right. Everything
// else (permissions, session id, cost) lives behind :status / :cost.
fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let right_txt = format!("{} ", app.model_display);
    let right_w = right_txt.width() as u16 + 1;
    let cols =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right_w)]).split(area);

    let title = space_name(app.space());
    let title_max = (cols[0].width as usize).saturating_sub(12);
    let left = Line::from(vec![
        Span::styled(
            " ✦ aeovim ".to_string(),
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ".to_string(), Style::default().fg(th.gutter)),
        Span::styled(
            truncate_width(&title, title_max.max(8)),
            Style::default().fg(th.fg),
        ),
    ]);
    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            right_txt,
            Style::default().fg(th.dim),
        )))
        .right_aligned(),
        cols[1],
    );
}

fn render_rule(f: &mut Frame, area: Rect) {
    let th = theme::get();
    let line: String = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, Style::default().fg(th.gutter)))),
        area,
    );
}

// ---- sidebar — the pane that came back ------------------------------------

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let focused = app.focus == Focus::Sidebar;
    let border = if focused { th.accent } else { th.gutter };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            " SPACES ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let running = app.running_count();
    let roll = if running > 0 {
        format!(" {} spaces · {} busy", app.spaces.len(), running)
    } else {
        format!(" {} spaces", app.spaces.len())
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(roll, Style::default().fg(th.dim))));

    let iw = inner.width as usize;
    for (i, sp) in app.spaces.iter().enumerate() {
        let is_active = i == app.active;
        let is_cursor = focused && i == app.sidebar_cursor;
        let renaming = app.mode == Mode::Rename && app.rename_target == i;

        let marker = if is_active {
            "▎"
        } else if is_cursor {
            "›"
        } else {
            " "
        };
        let num = match i {
            0..=8 => ((b'1' + i as u8) as char).to_string(),
            9 => "0".to_string(),
            _ => " ".to_string(),
        };
        let (glyph, gcol, meta) =
            status_glyph(sp.status(), space_running_secs(sp), app.spinner);

        let row_bg = if is_active {
            Some(th.sel)
        } else if is_cursor {
            Some(th.cursorline)
        } else {
            None
        };
        let mut name_st = Style::default().fg(if is_active { th.fg } else { th.fgdim });
        if is_active || is_cursor {
            name_st = name_st.add_modifier(Modifier::BOLD);
        }
        let mut name = if renaming {
            format!("{}▌", app.rename_buf)
        } else {
            space_name(sp)
        };
        if renaming {
            name_st = Style::default().fg(th.accent2).add_modifier(Modifier::BOLD);
        }
        // The pane count survives truncation — appended AFTER the name is cut.
        let count = if !renaming && sp.chats.len() > 1 {
            format!(" ({})", sp.chats.len())
        } else {
            String::new()
        };
        // marker(1)+sp + num(1)+sp + glyph(1)+sp = 6 cols; meta sits right.
        let meta_w = meta.width();
        let name_max = iw.saturating_sub(6 + meta_w + count.width() + 2);
        name = truncate_width(&name, name_max.max(4));
        name.push_str(&count);
        let gap = iw
            .saturating_sub(6 + name.width() + meta_w + 1)
            .max(1);

        let apply = |st: Style| match row_bg {
            Some(bg) => st.bg(bg),
            None => st,
        };
        let marker_col = if is_active { th.accent } else { th.accent2 };
        let meta_col = match sp.status() {
            ChatStatus::Running => th.info,
            ChatStatus::Error => th.err,
            _ => th.dim,
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), apply(Style::default().fg(marker_col))),
            Span::styled(format!("{num} "), apply(Style::default().fg(th.num))),
            Span::styled(format!("{glyph} "), apply(Style::default().fg(gcol))),
            Span::styled(name, apply(name_st)),
            Span::styled(" ".repeat(gap), apply(Style::default())),
            Span::styled(meta, apply(Style::default().fg(meta_col))),
            Span::styled(" ".to_string(), apply(Style::default())),
        ]));
    }

    let list_h = inner.height.saturating_sub(2);
    let list = Rect { height: list_h, ..inner };
    f.render_widget(Paragraph::new(lines), list);

    // bottom hints — kept to two quiet lines
    let hints = Rect {
        y: inner.y + inner.height - 2,
        height: 2,
        ..inner
    };
    let hint_st = Style::default().fg(th.dim);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" Space ee · Space 1-0 · ga", hint_st)),
            Line::from(Span::styled(" n new · d delete · r name", hint_st)),
        ]),
        hints,
    );
}

// ---- main region: one chat, or a two-pane thin-divider vsplit --------------

fn render_main(f: &mut Frame, rect: Rect, app: &mut App) {
    let spin = app.spinner;
    let active = app.active;
    let n = app.spaces[active].chats.len();
    if n <= 1 {
        let chat = &mut app.spaces[active].chats[0];
        render_transcript(f, rect, chat, spin);
        return;
    }

    // Two panes with a one-column divider; each pane gets a slim header line.
    let th = theme::get();
    let pane_w = rect.width.saturating_sub(1) / 2;
    let rects = [
        Rect { width: pane_w, ..rect },
        Rect {
            x: rect.x + pane_w + 1,
            width: rect.width.saturating_sub(pane_w + 1),
            ..rect
        },
    ];
    let divider = Rect {
        x: rect.x + pane_w,
        width: 1,
        ..rect
    };
    let div_lines: Vec<Line> = (0..rect.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(th.gutter))))
        .collect();
    f.render_widget(Paragraph::new(div_lines), divider);

    let focused_pane = app.spaces[active].focused.min(n - 1);
    let main_focus = app.focus == Focus::Main;
    for (ci, prect) in rects.iter().enumerate() {
        if prect.width < 8 || prect.height < 2 {
            continue;
        }
        let is_focused = main_focus && ci == focused_pane;
        // pane header: `▎ chat N` + status glyph
        let (hst, bar) = if is_focused {
            (
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                th.accent,
            )
        } else {
            (Style::default().fg(th.dim), th.gutter)
        };
        let status = app.spaces[active].chats[ci].status();
        let secs = app.spaces[active].chats[ci]
            .turn_started
            .map(|t| t.elapsed().as_secs());
        let (glyph, gcol, _) = status_glyph(status, secs, spin);
        let header = Rect { height: 1, ..*prect };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("▎", Style::default().fg(bar)),
                Span::styled(format!(" chat {} ", ci + 1), hst),
                Span::styled(glyph.to_string(), Style::default().fg(gcol)),
            ])),
            header,
        );
        let body = Rect {
            y: prect.y + 1,
            height: prect.height - 1,
            ..*prect
        };
        let chat = &mut app.spaces[active].chats[ci];
        render_transcript(f, body, chat, spin);
    }
}

// ---- transcript ------------------------------------------------------------

fn render_transcript(f: &mut Frame, rect: Rect, chat: &mut Chat, spin: usize) {
    // One column of breathing room each side; content never touches the edge.
    let body = Rect {
        x: rect.x + 1,
        y: rect.y,
        width: rect.width.saturating_sub(2),
        height: rect.height,
    };
    if chat.transcript.is_empty() && chat.streaming.is_none() && !chat.in_flight {
        render_welcome(f, body);
        // keep the cache/scroll state consistent even on the welcome screen
        chat.last_max_scroll = 0;
        return;
    }

    let width = body.width.max(1);
    let h = (body.height as usize).max(1);

    // Settled transcript: served pre-wrapped from the cache; rebuilt only when
    // the transcript, the width, or the fold toggle changed.
    if chat.cache.rev != chat.rev
        || chat.cache.width != width
        || chat.cache.expand != chat.expand_tools
    {
        let logical = build_transcript_lines(chat);
        chat.cache.rows = wrap_lines(&logical, width as usize);
        chat.cache.rev = chat.rev;
        chat.cache.width = width;
        chat.cache.expand = chat.expand_tools;
    }

    // Live tail (streaming text / working line / queue): small, rebuilt per frame.
    let tail = wrap_lines(&build_tail_lines(chat, spin), width as usize);

    let base_rows = chat.cache.rows.len();
    let total = base_rows + tail.len() + 1; // +1 bottom margin
    let max_scroll = total.saturating_sub(h);
    chat.last_max_scroll = max_scroll;
    let scroll = if chat.follow {
        max_scroll
    } else {
        chat.scroll.min(max_scroll)
    };

    // Only the rows in view are cloned — never the whole transcript.
    let mut visible: Vec<Line> = Vec::with_capacity(h);
    if total < h {
        // few rows → anchor to the bottom with blank space above (terminal style)
        for _ in 0..(h - total) {
            visible.push(Line::from(""));
        }
    }
    let end = (scroll + h).min(total);
    for r in scroll..end {
        if r < base_rows {
            visible.push(chat.cache.rows[r].clone());
        } else if r < base_rows + tail.len() {
            visible.push(tail[r - base_rows].clone());
        } else {
            visible.push(Line::from(""));
        }
    }
    f.render_widget(Paragraph::new(visible), body);

    // Scroll position hint when detached from the tail.
    if !chat.follow && max_scroll > 0 {
        let th = theme::get();
        let pct = (scroll * 100 / max_scroll.max(1)).min(99);
        let tag = format!(" ↕ {pct}% · G bottom ");
        let w = tag.width() as u16;
        if body.width > w + 2 {
            let r = Rect {
                x: body.x + body.width - w - 1,
                y: body.y,
                width: w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    tag,
                    Style::default().fg(th.dim),
                ))),
                r,
            );
        }
    }
}

fn render_welcome(f: &mut Frame, area: Rect) {
    let th = theme::get();
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let lines: Vec<(&str, Style)> = vec![
        ("✦ aeovim", bold(th.accent)),
        ("", Style::default()),
        ("spaces on the left · claude code underneath", Style::default().fg(th.dim)),
        ("", Style::default()),
        ("just type — Enter sends · Esc for vim", Style::default().fg(th.fg)),
        ("? keys", Style::default().fg(th.dim)),
    ];
    let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    for (i, (txt, st)) in lines.iter().enumerate() {
        let y = top + i as u16;
        if y >= area.y + area.height {
            break;
        }
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(txt.to_string(), *st)).centered()),
            Rect { x: area.x, y, width: area.width, height: 1 },
        );
    }
}

/// Logical (unwrapped) lines for every settled transcript entry.
fn build_transcript_lines(chat: &Chat) -> Vec<Line<'static>> {
    let th = theme::get();
    let user_lbl = Style::default().fg(th.info).add_modifier(Modifier::BOLD);
    let asst_lbl = Style::default().fg(th.accent).add_modifier(Modifier::BOLD);
    let tool_st = Style::default().fg(th.warn);
    let note_st = Style::default().fg(th.dim);
    let err_st = Style::default().fg(th.err).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(th.fg);

    let mut out: Vec<Line> = Vec::new();
    for (i, e) in chat.transcript.iter().enumerate() {
        // A tool result hugs the tool call above it (no separator), matching
        // Claude Code's `● Tool(...)` / `  ⎿ result` grouping. Everything else
        // gets a blank line, and a new user turn gets an extra one to group.
        if i > 0 && !matches!(e, Entry::ToolResult { .. } | Entry::Diff { .. }) {
            out.push(Line::from(""));
            if matches!(e, Entry::User(_)) {
                out.push(Line::from(""));
            }
        }
        match e {
            Entry::User(x) => push_block(&mut out, "▎ you", user_lbl, x, body),
            Entry::Assistant(x) => push_block(&mut out, "▎ claude", asst_lbl, x, body),
            Entry::Tool(x) => {
                for (j, l) in x.split('\n').enumerate() {
                    let l = crate::app::clean_line(l);
                    if j == 0 {
                        out.push(Line::from(Span::styled(
                            format!("  {l}"),
                            tool_st.add_modifier(Modifier::BOLD),
                        )));
                    } else {
                        let prefix = if j == 1 { "  ⎿  " } else { "     " };
                        out.push(Line::from(Span::styled(
                            format!("{prefix}{l}"),
                            Style::default().fg(th.dim),
                        )));
                    }
                }
            }
            Entry::ToolResult { ok, text } => {
                push_tool_result(&mut out, *ok, text, chat.expand_tools);
            }
            Entry::Diff { added, removed, lines, .. } => {
                // ⎿ summary, then the colored +/- hunk aligned under it.
                out.push(Line::from(Span::styled(
                    format!("  ⎿  +{added} -{removed}"),
                    Style::default().fg(th.gutter),
                )));
                for dl in lines {
                    let (marker, col) = match dl.kind {
                        DiffKind::Add => ("+ ", th.ok),
                        DiffKind::Del => ("- ", th.err),
                        DiffKind::Gap => ("  ", th.gutter),
                        DiffKind::Ctx => ("  ", th.dim),
                    };
                    out.push(Line::from(Span::styled(
                        format!("     {marker}{}", dl.text),
                        Style::default().fg(col),
                    )));
                }
            }
            Entry::Note(x) => {
                for l in x.split('\n') {
                    out.push(Line::from(Span::styled(
                        format!("  · {}", crate::app::clean_line(l)),
                        note_st,
                    )));
                }
            }
            Entry::Error(x) => push_block(&mut out, "▎ ✗ error", err_st, x, err_st),
        }
    }
    out
}

/// A tool result: one summary line collapsed (Ctrl-t expands), full text expanded.
/// The full text is stored either way — collapsing is a view, not a data loss.
fn push_tool_result(out: &mut Vec<Line<'static>>, ok: bool, text: &str, expand: bool) {
    let th = theme::get();
    let col = if ok { th.gutter } else { th.err };
    let lines: Vec<&str> = text.lines().collect();
    if !expand {
        let first = lines
            .iter()
            .find(|l| !l.trim().is_empty())
            .copied()
            .unwrap_or("");
        let mut summary = truncate_width(&crate::app::clean_line(first), 72);
        let extra = lines.len().saturating_sub(1);
        if extra > 0 {
            summary.push_str(&format!("  (+{extra} lines · Ctrl-t expands)"));
        }
        if !summary.is_empty() {
            out.push(Line::from(Span::styled(
                format!("  ⎿  {summary}"),
                Style::default().fg(col),
            )));
        }
        return;
    }
    for (i, l) in lines.iter().enumerate() {
        let prefix = if i == 0 { "  ⎿  " } else { "     " };
        out.push(Line::from(Span::styled(
            format!("{prefix}{}", crate::app::clean_line(l)),
            Style::default().fg(col),
        )));
    }
    if lines.is_empty() {
        out.push(Line::from(Span::styled(
            "  ⎿  (no output)".to_string(),
            Style::default().fg(col),
        )));
    }
}

/// The live tail: streaming text (fence-aware), the working line, queued prompts.
fn build_tail_lines(chat: &Chat, spin: usize) -> Vec<Line<'static>> {
    let th = theme::get();
    let asst_lbl = Style::default().fg(th.accent).add_modifier(Modifier::BOLD);
    let note_st = Style::default().fg(th.dim);
    let body = Style::default().fg(th.fg);
    let mut out: Vec<Line> = Vec::new();

    if let Some(s) = &chat.streaming {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled("▎ claude", asst_lbl)));
        // Same fence-tracked renderer as settled text — streamed code used to
        // render mangled and then visibly rewrite itself once committed.
        push_body(&mut out, s, body);
        if let Some(last) = out.last_mut() {
            last.spans
                .push(Span::styled("▌".to_string(), Style::default().fg(th.accent2)));
        }
    } else if chat.in_flight {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        // Working line: rolling wave · what's running · live seconds.
        let secs = chat.turn_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let label = chat.activity.as_deref().unwrap_or("Working");
        let mut spans: Vec<Span> = vec![Span::styled("  ".to_string(), note_st)];
        spans.extend(wave_spans(spin));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(th.accent2),
        ));
        spans.push(Span::styled(format!("  {}", fmt_secs(secs)), note_st));
        spans.push(Span::styled("   ctrl-c interrupt".to_string(), note_st));
        out.push(Line::from(spans));
    }

    // Prompts queued while the turn runs — shown dimmed below the active work.
    if !chat.queue.is_empty() {
        out.push(Line::from(""));
        for q in &chat.queue {
            out.push(Line::from(vec![
                Span::styled("  ❯ ".to_string(), Style::default().fg(th.accent2)),
                Span::styled(q.clone(), Style::default().fg(th.dim)),
                Span::styled(
                    "  (queued)".to_string(),
                    Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    out
}

// ---- width-aware wrapping ------------------------------------------------

/// Word-wrap styled lines to `width` display columns. Continuation rows start
/// at column 0, and the math is display-width-correct for CJK/emoji.
fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        wrap_line_into(l, width, &mut out);
    }
    out
}

fn line_width(l: &Line) -> usize {
    l.spans.iter().map(|s| s.content.width()).sum()
}

fn wrap_line_into(line: &Line<'static>, width: usize, out: &mut Vec<Line<'static>>) {
    let width = width.max(1);
    if line_width(line) <= width {
        out.push(line.clone());
        return;
    }
    // Flatten to a (char, style) stream, then greedily fill rows, breaking at
    // the last space in the row when one exists.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for sp in &line.spans {
        for c in sp.content.chars() {
            chars.push((c, sp.style));
        }
    }
    let mut row_start = 0usize;
    let mut w = 0usize;
    let mut last_space: Option<usize> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let cw = chars[i].0.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            let (break_at, resume_at) = if chars[i].0 == ' ' {
                // the overflowing char IS the word gap — end the row here
                (i, i + 1)
            } else {
                match last_space {
                    // break after the last space, dropping it from the row edge
                    Some(s) if s > row_start => (s, s + 1),
                    _ => (i, i), // one unbroken run — hard break
                }
            };
            push_row(&chars[row_start..break_at], out);
            row_start = resume_at;
            last_space = None;
            if resume_at > i {
                // consumed the current (space) char with the break
                i = resume_at;
                w = 0;
                continue;
            }
            w = chars[row_start..i]
                .iter()
                .map(|(c, _)| c.width().unwrap_or(0))
                .sum();
            // fall through — current char still pending
        }
        if chars[i].0 == ' ' {
            last_space = Some(i);
        }
        w += cw;
        i += 1;
    }
    if row_start < chars.len() || out.is_empty() {
        push_row(&chars[row_start..], out);
    }
}

/// Regroup a run of styled chars into spans (merging equal styles).
fn push_row(chars: &[(char, Style)], out: &mut Vec<Line<'static>>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (c, st) in chars {
        match cur {
            Some(s) if s == *st => buf.push(*c),
            Some(s) => {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
                buf.push(*c);
                cur = Some(*st);
            }
            None => {
                buf.push(*c);
                cur = Some(*st);
            }
        }
    }
    if let Some(s) = cur {
        if !buf.is_empty() {
            spans.push(Span::styled(buf, s));
        }
    }
    out.push(Line::from(spans));
}

// ---- inline markdown -----------------------------------------------------

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

fn is_word(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || *c == '_')
}

/// Small inline markdown: **bold**, *italic* / _italic_, `code`.
/// Emphasis needs word boundaries — `foo_bar_baz` and `a * b * c` stay
/// literal (the old pass ate the delimiters out of every snake_case name).
fn md_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let th = theme::get();
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let code_st = Style::default().fg(th.info);
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
                let inner_ok = s.get(i + 2).is_some_and(|c| !c.is_whitespace())
                    && j > i + 2
                    && !s[j - 1].is_whitespace();
                if inner_ok {
                    md_flush(&mut out, &mut buf, base);
                    out.push(Span::styled(
                        s[i + 2..j].iter().collect::<String>(),
                        base.add_modifier(Modifier::BOLD),
                    ));
                    i = j + 2;
                    continue;
                }
            }
        }
        if s[i] == '*' || s[i] == '_' {
            let d = s[i];
            // opener must sit at a word boundary with content right after it
            let opener_ok = !is_word(if i == 0 { None } else { s.get(i - 1) })
                && s.get(i + 1).is_some_and(|c| !c.is_whitespace() && *c != d);
            if opener_ok {
                if let Some(j) = (i + 1..n).find(|&k| s[k] == d) {
                    // closer must end at a word boundary with content before it
                    let closer_ok = j > i + 1
                        && !s[j - 1].is_whitespace()
                        && !is_word(s.get(j + 1));
                    if closer_ok {
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
        }
        buf.push(s[i]);
        i += 1;
    }
    md_flush(&mut out, &mut buf, base);
    out
}

/// A transcript body line: indent prefix + markdown (headers rendered bold).
fn md_line(prefix: &str, text: &str, base: Style) -> Line<'static> {
    let th = theme::get();
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
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    // bullet lists
    let bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "));
    if let Some(rest) = bullet {
        let mut spans = vec![Span::styled(format!("{prefix}• "), Style::default().fg(th.info))];
        spans.extend(md_spans(rest, base));
        return Line::from(spans);
    }
    // blockquote
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return Line::from(vec![
            Span::styled(format!("{prefix}▏ "), Style::default().fg(th.gutter)),
            Span::styled(
                rest.to_string(),
                Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
            ),
        ]);
    }
    let mut spans = vec![Span::styled(prefix.to_string(), base)];
    spans.extend(md_spans(text, base));
    Line::from(spans)
}

/// Render message body text with ``` fence tracking — code inside fences gets a
/// gutter bar and is never markdown-mangled. Shared by settled AND streaming.
fn push_body(out: &mut Vec<Line<'static>>, text: &str, body: Style) {
    let th = theme::get();
    let mut in_fence = false;
    for l in text.split('\n') {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
            let lang = l.trim_start().trim_start_matches('`').trim();
            let tag = if in_fence && !lang.is_empty() {
                format!("  ▏ {lang}")
            } else {
                "  ▏".to_string()
            };
            out.push(Line::from(Span::styled(tag, Style::default().fg(th.gutter))));
            continue;
        }
        if in_fence {
            out.push(Line::from(vec![
                Span::styled("  ▏ ".to_string(), Style::default().fg(th.gutter)),
                Span::styled(crate::app::clean_line(l), Style::default().fg(th.info)),
            ]));
        } else {
            out.push(md_line("  ", l, body));
        }
    }
}

fn push_block(out: &mut Vec<Line<'static>>, label: &str, lbl: Style, text: &str, body: Style) {
    out.push(Line::from(Span::styled(label.to_string(), lbl)));
    push_body(out, text, body);
}

// ---- composer ------------------------------------------------------------

fn render_slash_popup(f: &mut Frame, composer: Rect, app: &App) {
    let th = theme::get();
    let area = f.area();
    let matches = app.slash_matches();
    // Clamp to the space above the composer — an unclamped rect used to panic
    // ratatui's buffer indexing in short terminals.
    let space_above = composer.y.saturating_sub(area.y);
    if space_above < 3 {
        return;
    }
    let shown = (matches.len().clamp(1, 8) as u16).min(space_above.saturating_sub(2));
    let h = shown + 2;
    let w = 40u16.min(composer.width).min(area.width);
    let y = composer.y - h;
    let x = composer.x.min(area.right().saturating_sub(w));
    let rect = Rect { x, y, width: w, height: h };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(
            " commands — Tab completes ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
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
                Style::default().fg(th.accent2).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.fg)
            };
            Line::from(Span::styled(format!("{caret}/{cmd}"), st))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// The composer's editable text for the current mode, plus its prompt prefix
/// and the cursor byte offset within the text.
fn composer_edit(app: &App) -> Option<(&'static str, &str, usize)> {
    match app.mode {
        Mode::Insert => Some(("❯ ", app.input.as_str(), app.input_cursor)),
        Mode::Rename => Some(("rename ❯ ", app.rename_buf.as_str(), app.rename_buf.len())),
        Mode::Command => Some((":", app.cmdline.as_str(), app.cmdline.len())),
        _ => None,
    }
}

/// Hard-wrap `text` (may contain '\n') into visual rows of at most `width`
/// display columns, tracking where the cursor (a byte offset) lands.
/// Row 0 is offset by `first_indent` (the prompt prefix).
struct WrappedInput {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: u16, // display column within the row (prefix included on row 0)
}

fn wrap_input(text: &str, cursor: usize, first_indent: u16, width: u16) -> WrappedInput {
    let width = width.max(2) as usize;
    let indent = (first_indent as usize).min(width.saturating_sub(1));
    let cursor = cursor.min(text.len());
    let mut rows: Vec<String> = vec![String::new()];
    let mut w = indent; // row 0 starts after the prefix
    let mut cur = (0usize, indent as u16);
    let mut byte = 0usize;
    for c in text.chars() {
        if byte == cursor {
            cur = (rows.len() - 1, w as u16);
        }
        if c == '\n' {
            rows.push(String::new());
            w = 0;
        } else {
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                rows.push(String::new());
                w = 0;
            }
            rows.last_mut().unwrap().push(c);
            w += cw;
        }
        byte += c.len_utf8();
    }
    if cursor >= text.len() {
        cur = (rows.len() - 1, w as u16);
    }
    WrappedInput {
        rows,
        cursor_row: cur.0,
        cursor_col: cur.1,
    }
}

/// Height (incl. borders) the composer needs for its current content, capped so
/// it never eats more than half the screen — past that it scrolls internally.
fn composer_height(app: &App, inner_w: u16, screen_h: u16) -> u16 {
    let Some((prefix, text, cursor)) = composer_edit(app) else {
        return 3; // single hint line + borders
    };
    let rows = wrap_input(text, cursor, prefix.width() as u16, inner_w).rows.len() as u16;
    let max_content = (screen_h / 2).max(1);
    rows.clamp(1, max_content) + 2
}

fn render_composer(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let accent = match app.mode {
        Mode::Insert => th.accent2,
        Mode::Rename => th.accent,
        Mode::Command => th.warn,
        _ => th.gutter,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some((prefix, text, cursor)) = composer_edit(app) {
        let prefix_w = prefix.width() as u16;
        let wrapped = wrap_input(text, cursor, prefix_w, inner.width);
        // The visible window follows the CURSOR row.
        let win = (inner.height as usize).max(1);
        let start = if wrapped.cursor_row >= win {
            wrapped.cursor_row + 1 - win
        } else {
            0
        };
        let mut lines: Vec<Line> = Vec::new();
        for (vi, r) in wrapped.rows.iter().enumerate().skip(start).take(win) {
            if vi == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(accent)),
                    Span::styled(r.clone(), Style::default().fg(th.fg)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    r.clone(),
                    Style::default().fg(th.fg),
                )));
            }
        }
        // Empty Insert composer: show a gentle placeholder after the prompt.
        if app.mode == Mode::Insert && text.is_empty() {
            if let Some(first) = lines.first_mut() {
                first.spans.push(Span::styled(
                    "ask anything — / for commands".to_string(),
                    Style::default().fg(th.gutter),
                ));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);

        let x = (inner.x + wrapped.cursor_col).min(inner.x + inner.width.saturating_sub(1));
        let y = inner.y + ((wrapped.cursor_row - start) as u16).min(inner.height.saturating_sub(1));
        f.set_cursor_position(Position::new(x, y));
        return;
    }

    // Normal/Confirm: keep the drafted text visible dimmed, else the key hints.
    let line = if app.input.is_empty() {
        Line::from(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(th.accent2)),
            Span::styled(
                "i to compose · : commands · ? keys".to_string(),
                Style::default().fg(th.gutter),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(th.dim)),
            Span::styled(app.input.replace('\n', " ⏎ "), Style::default().fg(th.dim)),
            Span::styled("  — i to edit".to_string(), Style::default().fg(th.gutter)),
        ])
    };
    f.render_widget(Paragraph::new(line), inner);
}

// ---- lualine-style powerline statusline -----------------------------------

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let (label, mode_col) = match app.mode {
        Mode::Normal => ("NORMAL", th.mode_normal),
        Mode::Insert => ("INSERT", th.mode_insert),
        Mode::Rename => ("RENAME", th.mode_visual),
        Mode::Command => ("COMMAND", th.mode_command),
        Mode::Confirm => ("CONFIRM", th.err),
    };
    // Codex-simple: just [ MODE ][ name ]. A toast takes the name segment
    // until the next keypress; cost/model/permissions live in :cost / :status.
    let seg1 = match &app.toast {
        Some(m) => format!(" {} ", truncate_width(m, 60)),
        None => format!(" {} ", truncate_width(&space_name(app.space()), 32)),
    };
    let seg1_st = if app.toast.is_some() {
        Style::default().fg(th.warn).bg(th.panel)
    } else {
        Style::default().fg(th.fg).bg(th.panel)
    };

    // No right-side loader — the transcript's working line already carries the
    // wave + elapsed; the statusline stays quiet.
    let left = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(th.panel)
                .bg(mode_col)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(SEP_R, Style::default().fg(mode_col).bg(th.panel)),
        Span::styled(seg1, seg1_st),
        Span::styled(SEP_R, Style::default().fg(th.panel)),
    ]);
    f.render_widget(Paragraph::new(left), area);
}

// ---- overlays --------------------------------------------------------------

/// `ga` — fuzzy space picker (GO TO SPACE).
fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let matches = app.picker_matches();
    let shown = matches.len().clamp(1, 8) as u16;
    let w = 72u16.min(area.width.saturating_sub(4));
    let h = (shown + 4).min(area.height.saturating_sub(2));
    let mut rect = centered(area, w, h);
    rect.y = (area.y + area.height / 5).min(rect.y);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(
            " GO TO SPACE ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.height < 2 {
        return;
    }

    let count = format!("{}/{}", matches.len(), app.spaces.len());
    let qpad = (inner.width as usize)
        .saturating_sub(2 + app.picker_query.width() + 1 + count.width() + 1);
    let query = Line::from(vec![
        Span::styled("❯ ".to_string(), Style::default().fg(th.accent2).add_modifier(Modifier::BOLD)),
        Span::styled(app.picker_query.clone(), Style::default().fg(th.fg)),
        Span::styled("▌".to_string(), Style::default().fg(th.accent2)),
        Span::styled(" ".repeat(qpad.max(1)), Style::default()),
        Span::styled(count, Style::default().fg(th.dim)),
    ]);
    f.render_widget(query, Rect { height: 1, ..inner });

    let sel = app.picker_sel.min(matches.len().saturating_sub(1));
    let list = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    let win = list.height as usize;
    let start = if sel >= win { sel + 1 - win } else { 0 };
    let mut lines: Vec<Line> = Vec::new();
    for (row, (idx, pos)) in matches.iter().enumerate().skip(start).take(win) {
        let sp = &app.spaces[*idx];
        let selected = row == sel;
        let row_bg = if selected { Some(th.sel) } else { None };
        let apply = |st: Style| match row_bg {
            Some(bg) => st.bg(bg),
            None => st,
        };
        let caret = if selected { "› " } else { "  " };
        let num = match *idx {
            0..=8 => ((b'1' + *idx as u8) as char).to_string(),
            9 => "0".to_string(),
            _ => " ".to_string(),
        };
        let (glyph, gcol, meta) =
            status_glyph(sp.status(), space_running_secs(sp), app.spinner);

        let mut spans: Vec<Span> = vec![
            Span::styled(caret.to_string(), apply(Style::default().fg(th.accent2))),
            Span::styled(format!("{num} "), apply(Style::default().fg(th.num))),
            Span::styled(format!("{glyph} "), apply(Style::default().fg(gcol))),
        ];
        // name with matched chars highlighted (fuzzy hits in accent2 bold)
        let title = truncate_width(&space_name(sp), (inner.width as usize).saturating_sub(24));
        let plain = apply(Style::default().fg(if selected { th.fg } else { th.fgdim }));
        let hit = apply(
            Style::default()
                .fg(th.accent2)
                .add_modifier(Modifier::BOLD),
        );
        let mut buf = String::new();
        let mut buf_hit = false;
        for (ci, ch) in title.chars().enumerate() {
            let is_hit = pos.contains(&ci);
            if is_hit != buf_hit && !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    if buf_hit { hit } else { plain },
                ));
            }
            buf_hit = is_hit;
            buf.push(ch);
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, if buf_hit { hit } else { plain }));
        }
        // status word on the right edge
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let gap = (inner.width as usize).saturating_sub(used + meta.width() + 1);
        spans.push(Span::styled(" ".repeat(gap.max(1)), apply(Style::default())));
        spans.push(Span::styled(meta, apply(Style::default().fg(th.dim))));
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), list);
}

/// `gd` / bare `:cd` — fuzzy directory picker rooted at $HOME. Same shape as
/// the space picker so the two feel like one control; the current dir of the
/// space is shown in the title, since that's what you're about to change.
fn render_dir_picker(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let matches = app.dir_matches();
    let shown = matches.len().clamp(1, 12) as u16;
    let w = 78u16.min(area.width.saturating_sub(4));
    let h = (shown + 4).min(area.height.saturating_sub(2));
    let mut rect = centered(area, w, h);
    rect.y = (area.y + area.height / 6).min(rect.y);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(
            format!(" OPEN DIR — now in {} ", crate::app::tilde(&app.space().dir)),
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.height < 2 {
        return;
    }

    let count = format!("{}/{}", matches.len(), app.dir_candidates.len());
    let qpad = (inner.width as usize)
        .saturating_sub(2 + app.dir_query.width() + 1 + count.width() + 1);
    let query = Line::from(vec![
        Span::styled(
            "❯ ".to_string(),
            Style::default().fg(th.accent2).add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.dir_query.clone(), Style::default().fg(th.fg)),
        Span::styled("▌".to_string(), Style::default().fg(th.accent2)),
        Span::styled(" ".repeat(qpad.max(1)), Style::default()),
        Span::styled(count, Style::default().fg(th.dim)),
    ]);
    f.render_widget(query, Rect { height: 1, ..inner });

    let sel = app.dir_sel.min(matches.len().saturating_sub(1));
    let list = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
    let win = list.height as usize;
    let start = if sel >= win { sel + 1 - win } else { 0 };
    let mut lines: Vec<Line> = Vec::new();
    for (row, (idx, pos)) in matches.iter().enumerate().skip(start).take(win) {
        let selected = row == sel;
        let row_bg = if selected { Some(th.sel) } else { None };
        let apply = |st: Style| match row_bg {
            Some(bg) => st.bg(bg),
            None => st,
        };
        let caret = if selected { "› " } else { "  " };
        let mut spans: Vec<Span> = vec![Span::styled(
            caret.to_string(),
            apply(Style::default().fg(th.accent2)),
        )];
        let path = crate::app::tilde(&app.dir_candidates[*idx]);
        let path = truncate_width(&path, (inner.width as usize).saturating_sub(4));
        let plain = apply(Style::default().fg(if selected { th.fg } else { th.fgdim }));
        let hit = apply(Style::default().fg(th.accent2).add_modifier(Modifier::BOLD));
        let mut buf = String::new();
        let mut buf_hit = false;
        for (ci, ch) in path.chars().enumerate() {
            let is_hit = pos.contains(&ci);
            if is_hit != buf_hit && !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    if buf_hit { hit } else { plain },
                ));
            }
            buf_hit = is_hit;
            buf.push(ch);
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, if buf_hit { hit } else { plain }));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), list);
}

/// `:tasks` — one row per chat (kept simple: space, state, elapsed, last).
fn render_board(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let rows = app.board_rows();
    let w = 92u16.min(area.width.saturating_sub(4));
    let h = (rows.len() as u16 + 5).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);
    let title = format!(" TASKS — {} busy · {} spaces ", app.running_count(), app.spaces.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(
            title,
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.height < 3 {
        return;
    }

    let name_w = 26usize.min((inner.width as usize).saturating_sub(34)).max(10);
    let head_st = Style::default().fg(th.dim);
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!(
            " {}{}{}{}last",
            cell("#", 4),
            cell("space", name_w + 2),
            cell("state", 12),
            cell("elapsed", 9),
        ),
        head_st,
    ))];

    for (row, (si, ci)) in rows.iter().enumerate() {
        let sp = &app.spaces[*si];
        let c = &sp.chats[*ci];
        let selected = row == app.board_sel;
        let row_bg = if selected { Some(th.sel) } else { None };
        let apply = |st: Style| match row_bg {
            Some(bg) => st.bg(bg),
            None => st,
        };
        let (state, scol) = match c.status() {
            ChatStatus::Running => (
                format!("{} running", SPIN[app.spinner % SPIN.len()]),
                th.accent,
            ),
            ChatStatus::Error => ("✗ error".to_string(), th.err),
            ChatStatus::Idle => ("✓ idle".to_string(), th.ok),
            ChatStatus::New => ("· new".to_string(), th.dim),
        };
        let elapsed = match (c.in_flight, c.turn_started) {
            (true, Some(t)) => fmt_secs(t.elapsed().as_secs()),
            _ => "—".to_string(),
        };
        // last line: what's running now, else the last transcript line.
        let last = if let Some(a) = &c.activity {
            a.clone()
        } else {
            c.transcript
                .iter()
                .rev()
                .find_map(|e| match e {
                    Entry::Assistant(x) | Entry::User(x) | Entry::Note(x) | Entry::Error(x) => {
                        x.lines().next().map(|l| l.to_string())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };
        let name = if sp.chats.len() > 1 {
            format!("{} · {}", space_name(sp), ci + 1)
        } else {
            space_name(sp)
        };
        let last_max = (inner.width as usize).saturating_sub(4 + name_w + 2 + 12 + 9 + 2);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}", cell(&format!("{}", si + 1), 4)),
                apply(Style::default().fg(th.num)),
            ),
            Span::styled(
                cell(&name, name_w + 2),
                apply(Style::default().fg(if selected { th.fg } else { th.fgdim })),
            ),
            Span::styled(cell(&state, 12), apply(Style::default().fg(scol))),
            Span::styled(cell(&elapsed, 9), apply(Style::default().fg(th.dim))),
            Span::styled(
                truncate_width(&crate::app::clean_line(&last), last_max.max(4)),
                apply(Style::default().fg(th.dim)),
            ),
        ]));
    }
    let list = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    f.render_widget(Paragraph::new(lines), list);

    let foot = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ⏎ focus · x cancel turn · esc close",
            Style::default().fg(th.dim),
        ))),
        foot,
    );
}

/// `:diff` — a quick pad over the focused chat's edit history. j/k scroll the
/// hunk; Ctrl-j/Ctrl-k step to the older / newer diff.
fn render_diffpad(f: &mut Frame, area: Rect, app: &mut App) {
    let th = theme::get();
    // Cheap: rebuilds only when the chat changed since the last build.
    app.sync_diff_doc();
    if app.diff_rows.is_empty() {
        app.overlay = Overlay::None;
        return;
    }
    let doc = &app.diff_rows;
    let starts = &app.diff_starts;

    let w = area.width.saturating_sub(8).clamp(40, 110);
    let h = area.height.saturating_sub(4).max(8);
    let rect = centered(area, w, h);

    let inner_h = h.saturating_sub(2) as usize; // block borders
    let body_h = inner_h.saturating_sub(1); // footer row
    let max_scroll = doc.len().saturating_sub(body_h.max(1));
    app.diff_scroll = app.diff_scroll.min(max_scroll);
    let scroll = app.diff_scroll;

    // The header names whatever row is at the TOP of the viewport, so
    // scrolling from one file's hunk into the next renames it in place.
    let head = &doc[scroll.min(doc.len() - 1)];
    let which = starts.iter().filter(|&&s| s <= scroll).count().max(1);
    let title = format!(
        " DIFF · {} · +{} −{} · file {}/{} ",
        head.file,
        head.added,
        head.removed,
        which,
        starts.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(
            truncate_width(&title, w as usize - 2),
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.height < 2 {
        return;
    }

    let mut out: Vec<Line> = Vec::with_capacity(body_h);
    for row in doc.iter().skip(scroll).take(body_h) {
        match &row.line {
            // File separator: the boundary you scroll across, spelled out in
            // the body too so it's obvious why the header just changed.
            None => out.push(Line::from(vec![
                Span::styled(
                    format!(" ▌ {} ", row.file),
                    Style::default()
                        .fg(th.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("+{} −{}", row.added, row.removed),
                    Style::default().fg(th.dim),
                ),
            ])),
            Some(dl) => {
                let (marker, col) = match dl.kind {
                    DiffKind::Add => ("+ ", th.ok),
                    DiffKind::Del => ("- ", th.err),
                    DiffKind::Gap => ("  ", th.gutter),
                    DiffKind::Ctx => ("  ", th.dim),
                };
                out.push(Line::from(Span::styled(
                    format!(" {marker}{}", dl.text),
                    Style::default().fg(col),
                )));
            }
        }
    }
    let body = Rect {
        height: inner.height - 1,
        ..inner
    };
    f.render_widget(Paragraph::new(out), body);

    let foot = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            &if app.diff_elided > 0 {
                // Never let the cap hide work silently.
                format!(
                    " j/k scroll · ⌃j/⌃k jump file · g/G top/end · esc close · {} older file(s) not shown",
                    app.diff_elided
                )
            } else {
                " j/k scroll · ⌃j/⌃k jump file · g/G top/end · esc close".to_string()
            },
            Style::default().fg(th.dim),
        ))),
        foot,
    );
}

fn render_confirm(f: &mut Frame, area: Rect, app: &App) {
    let th = theme::get();
    let msg = app.confirm_msg.clone();
    let w = (msg.width() as u16 + 6).clamp(24, area.width.saturating_sub(4).max(24));
    let rect = centered(area, w, 3);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.err))
        .title(Span::styled(
            " confirm ",
            Style::default().fg(th.err).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(th.fg),
        ))),
        inner,
    );
}

fn render_help(f: &mut Frame, area: Rect) {
    let th = theme::get();
    let rows: &[(&str, &str)] = &[
        ("", "MODES — Esc is Normal · i is Insert"),
        ("i / a / o", "compose (o = new line) · Enter sends, stays Insert"),
        ("Esc", "Insert → Normal · in Normal: snap to tail"),
        (":", ":q :vs :new :clear :diff :tasks :rename :cost :status :N"),
        ("/", "slash commands — Tab completes, Enter sends"),
        ("", "SPACES (sidebar) — a space holds 1–2 chats"),
        ("Space ee", "toggle sidebar · Space ef / Ctrl-h focus it"),
        ("Space 1-0", "jump to space N · gt / gT cycle · ga fuzzy"),
        ("Space n · Space t", "new space · tasks"),
        ("j k ⏎ n d r", "in sidebar: move · open · new · delete · rename"),
        ("", "SPLITS"),
        (":vs · Ctrl-w v", "second chat pane in this space"),
        ("Ctrl-h/l · Ctrl-w w", "move between panes (and the sidebar)"),
        (":q · Ctrl-w q", "close pane — then the space"),
        ("", "REVIEW"),
        (":diff", "diff pad · j/k scroll · ⌃j older · ⌃k newer"),
        ("Ctrl-t", "expand / collapse tool output"),
        ("", "STEER"),
        ("Ctrl-c", "interrupt (again = force) · idle: quit (asks)"),
        ("(while busy)", "keep typing — sends queue up in order"),
        ("? · F1", "this help"),
    ];

    let w = 64u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 2).min(area.height.saturating_sub(2).max(3));
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            " aeovim — keys  (any key to close) ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
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
                    Style::default().fg(th.info).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {k:>15}  "),
                        Style::default().fg(th.accent2).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(d.to_string(), Style::default().fg(th.fg)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Line<'static> {
        Line::from(Span::styled(s.to_string(), Style::default()))
    }

    fn row_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_short_line_passes_through() {
        let mut out = Vec::new();
        wrap_line_into(&plain("hello"), 10, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(row_text(&out[0]), "hello");
    }

    #[test]
    fn wrap_breaks_at_word_boundary() {
        let mut out = Vec::new();
        wrap_line_into(&plain("hello brave new world"), 11, &mut out);
        let rows: Vec<String> = out.iter().map(row_text).collect();
        assert!(rows.iter().all(|r| r.width() <= 11), "{rows:?}");
        assert_eq!(rows.join("|"), "hello brave|new world");
    }

    #[test]
    fn wrap_hard_breaks_unbroken_runs() {
        let mut out = Vec::new();
        wrap_line_into(&plain("aaaaaaaaaaaa"), 5, &mut out);
        let rows: Vec<String> = out.iter().map(row_text).collect();
        assert_eq!(rows, vec!["aaaaa", "aaaaa", "aa"]);
    }

    #[test]
    fn wrap_counts_display_width_for_cjk() {
        // Each CJK char is 2 columns — 3 chars per 6-wide row, not 6.
        let mut out = Vec::new();
        wrap_line_into(&plain("日本語も分かち"), 6, &mut out);
        let rows: Vec<String> = out.iter().map(row_text).collect();
        assert!(rows.iter().all(|r| r.width() <= 6), "{rows:?}");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn wrap_preserves_styles_across_break() {
        let th = theme::get();
        let l = Line::from(vec![
            Span::styled("red ".to_string(), Style::default().fg(th.err)),
            Span::styled("bluebluexx".to_string(), Style::default().fg(th.info)),
        ]);
        let mut out = Vec::new();
        wrap_line_into(&l, 8, &mut out);
        assert!(out.len() >= 2);
        // first row keeps the err span styled err
        assert_eq!(out[0].spans[0].style.fg, Some(th.err));
    }

    #[test]
    fn snake_case_is_not_italicised() {
        let spans = md_spans("call foo_bar_baz today", Style::default());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(joined, "call foo_bar_baz today");
    }

    #[test]
    fn glob_stars_are_not_italicised() {
        let spans = md_spans("check *.rs and *.toml files", Style::default());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(joined, "check *.rs and *.toml files");
    }

    #[test]
    fn real_emphasis_still_works() {
        let spans = md_spans("this is *important* stuff", Style::default());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(joined, "this is important stuff");
        assert!(spans
            .iter()
            .any(|s| s.content == "important" && s.style.add_modifier.contains(Modifier::ITALIC)));
        let spans = md_spans("very **bold** move", Style::default());
        assert!(spans
            .iter()
            .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn inline_code_wins_over_emphasis() {
        let spans = md_spans("run `cargo build --all_features` now", Style::default());
        assert!(spans.iter().any(|s| s.content == "cargo build --all_features"));
    }

    #[test]
    fn wrap_input_tracks_cursor() {
        // cursor mid-text on a wrapped row
        let w = wrap_input("hello world", 5, 2, 8);
        assert_eq!(w.cursor_row, 0);
        assert_eq!(w.cursor_col, 7); // 2 prefix + "hello"[..5]
        let w = wrap_input("ab\ncd", 4, 2, 8); // cursor on 'd'
        assert_eq!(w.cursor_row, 1);
        assert_eq!(w.cursor_col, 1);
        // cursor at end
        let w = wrap_input("ab", 2, 2, 8);
        assert_eq!((w.cursor_row, w.cursor_col), (0, 4));
    }

    #[test]
    fn truncate_width_is_display_width() {
        assert_eq!(truncate_width("hello", 10), "hello");
        let t = truncate_width("日本語テキスト", 6);
        assert!(t.width() <= 6, "{t} is {} wide", t.width());
        assert!(t.ends_with('…'));
    }

    #[test]
    fn cell_pads_to_exact_width() {
        assert_eq!(cell("ab", 5), "ab   ");
        assert_eq!(cell("abcdefgh", 5).width(), 5);
    }
}
