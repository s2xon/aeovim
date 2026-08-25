//! Rendering — single-session surface, themed with the lilac palette from the
//! user's nvim. Layout, top to bottom: header row · thin rule · open transcript
//! (no box) · powerline statusline · composer. DeepSeek-TUI/Claude-Code shape:
//! one conversation, everything visible, nothing to manage.
//!
//! Perf model (the CodeWhale lesson): the settled transcript is pre-wrapped
//! into visual rows ONCE per change (`RenderCache`, keyed by rev/width/expand)
//! and each frame only clones the ≤height rows in view. The streaming tail is
//! tiny and rebuilt per frame.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{chat_title, App, Chat, DiffKind, Entry, Mode, Pending};
use crate::theme as t;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SEP_R: &str = "\u{e0b0}"; // powerline right-filled

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

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let composer_h = composer_height(app, area.width.saturating_sub(4), area.height);
    let rows = Layout::vertical([
        Constraint::Length(1),          // header
        Constraint::Length(1),          // rule
        Constraint::Min(3),             // transcript
        Constraint::Length(1),          // lualine status
        Constraint::Length(composer_h), // prompt (flush to bottom, auto-grows)
    ])
    .split(area);

    render_header(f, rows[0], app);
    render_rule(f, rows[1]);
    render_transcript(f, rows[2], app);
    render_status(f, rows[3], app);
    render_composer(f, rows[4], app);

    if app.slash_active() {
        render_slash_popup(f, rows[4], app);
    }

    if app.help_open {
        render_help(f, area);
    } else if app.mode == Mode::Confirm {
        render_confirm(f, area, app);
    } else if app.pending != Pending::None {
        render_whichkey(f, area, app.pending);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let sid = &app.chat.session_id[..8.min(app.chat.session_id.len())];
    let perm = if app.dangerous { "dangerous" } else { "acceptEdits" };
    let right_txt = format!("{} · {perm} · {sid} ", app.model_display);
    let right_w = right_txt.width() as u16 + 1;
    let cols =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right_w)]).split(area);

    let title = chat_title(&app.chat);
    let title_max = (cols[0].width as usize).saturating_sub(12);
    let left = Line::from(vec![
        Span::styled(
            " ✦ aeovim ".to_string(),
            Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ".to_string(), Style::default().fg(t::GUTTER)),
        Span::styled(
            truncate_width(&title, title_max.max(8)),
            Style::default().fg(t::FG),
        ),
    ]);
    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            right_txt,
            Style::default().fg(t::DIM),
        )))
        .right_aligned(),
        cols[1],
    );
}

fn render_rule(f: &mut Frame, area: Rect) {
    let line: String = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, Style::default().fg(t::GUTTER)))),
        area,
    );
}

fn render_transcript(f: &mut Frame, rect: Rect, app: &mut App) {
    // One column of breathing room each side; content never touches the edge.
    let body = Rect {
        x: rect.x + 1,
        y: rect.y,
        width: rect.width.saturating_sub(2),
        height: rect.height,
    };
    let chat = &app.chat;
    if chat.transcript.is_empty() && chat.streaming.is_none() && !chat.in_flight {
        render_welcome(f, body);
        // keep the cache/scroll state consistent even on the welcome screen
        app.chat.last_max_scroll = 0;
        return;
    }

    let spin = app.spinner;
    let width = body.width.max(1);
    let h = (body.height as usize).max(1);
    let chat = &mut app.chat;

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
                    Style::default().fg(t::DIM),
                ))),
                r,
            );
        }
    }
}

fn render_welcome(f: &mut Frame, area: Rect) {
    let lines: Vec<(&str, Style)> = vec![
        ("✦ aeovim", Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD)),
        ("", Style::default()),
        ("one conversation · claude code underneath", Style::default().fg(t::DIM)),
        ("", Style::default()),
        ("just type — Enter sends, Shift-Enter is a newline", Style::default().fg(t::FG)),
        ("/  commands   ·   Esc  scroll mode   ·   ?  keys", Style::default().fg(t::DIM)),
        ("Esc interrupts a running turn · za expands tool output", Style::default().fg(t::DIM)),
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
    let user_lbl = Style::default().fg(t::PERI).add_modifier(Modifier::BOLD);
    let asst_lbl = Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD);
    let tool_st = Style::default().fg(t::AMBER);
    let note_st = Style::default().fg(t::DIM);
    let err_st = Style::default().fg(t::RED).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(t::FG);

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
            Entry::User(x) => push_block(&mut out, "❯ you", user_lbl, x, body),
            Entry::Assistant(x) => push_block(&mut out, "✦ claude", asst_lbl, x, body),
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
                            Style::default().fg(t::DIM),
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
                    Style::default().fg(t::GUTTER),
                )));
                for dl in lines {
                    let (marker, col) = match dl.kind {
                        DiffKind::Add => ("+ ", t::GREEN),
                        DiffKind::Del => ("- ", t::RED),
                        DiffKind::Gap => ("  ", t::GUTTER),
                        DiffKind::Ctx => ("  ", t::DIM),
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
            Entry::Error(x) => push_block(&mut out, "✗ error", err_st, x, err_st),
        }
    }
    out
}

/// A tool result: one summary line collapsed (za expands), full text expanded.
/// The full text is stored either way — collapsing is a view, not a data loss.
fn push_tool_result(out: &mut Vec<Line<'static>>, ok: bool, text: &str, expand: bool) {
    let col = if ok { t::GUTTER } else { t::RED };
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
            summary.push_str(&format!("  (+{extra} lines · za expands)"));
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
    let asst_lbl = Style::default().fg(t::PURPLE).add_modifier(Modifier::BOLD);
    let note_st = Style::default().fg(t::DIM);
    let body = Style::default().fg(t::FG);
    let mut out: Vec<Line> = Vec::new();

    if let Some(s) = &chat.streaming {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled("✦ claude", asst_lbl)));
        // Same fence-tracked renderer as settled text — streamed code used to
        // render mangled and then visibly rewrite itself once committed.
        push_body(&mut out, s, body);
        if let Some(last) = out.last_mut() {
            last.spans
                .push(Span::styled("▌".to_string(), Style::default().fg(t::PINK)));
        }
    } else if chat.in_flight {
        if !chat.transcript.is_empty() {
            out.push(Line::from(""));
        }
        // Claude-Code-style working line: spinner · what's running · live seconds.
        let secs = chat.turn_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let label = chat.activity.as_deref().unwrap_or("Working");
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", SPIN[spin % SPIN.len()]), asst_lbl),
            Span::styled(label.to_string(), body),
            Span::styled(format!("  {}", fmt_secs(secs)), note_st),
            Span::styled("   esc interrupt".to_string(), note_st),
        ]));
    }

    // Prompts queued while the turn runs — shown dimmed below the active work.
    if !chat.queue.is_empty() {
        out.push(Line::from(""));
        for q in &chat.queue {
            out.push(Line::from(vec![
                Span::styled("  ❯ ".to_string(), Style::default().fg(t::PINK)),
                Span::styled(q.clone(), Style::default().fg(t::DIM)),
                Span::styled(
                    "  (queued)".to_string(),
                    Style::default().fg(t::DIM).add_modifier(Modifier::ITALIC),
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
            Span::styled(
                rest.to_string(),
                Style::default().fg(t::DIM).add_modifier(Modifier::ITALIC),
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
            out.push(Line::from(Span::styled(tag, Style::default().fg(t::GUTTER))));
            continue;
        }
        if in_fence {
            out.push(Line::from(vec![
                Span::styled("  ▏ ".to_string(), Style::default().fg(t::GUTTER)),
                Span::styled(crate::app::clean_line(l), Style::default().fg(t::PERI)),
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
        .border_style(Style::default().fg(t::BORDER))
        .title(Span::styled(
            " commands — Tab completes ",
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

/// The composer's editable text for the current mode, plus its prompt prefix
/// and the cursor byte offset within the text.
fn composer_edit(app: &App) -> Option<(&'static str, &str, usize)> {
    match app.mode {
        Mode::Insert => Some(("❯ ", app.input.as_str(), app.input_cursor)),
        Mode::Command => Some((": ", app.cmd.as_str(), app.cmd.len())),
        Mode::Rename => Some(("rename ❯ ", app.rename_buf.as_str(), app.rename_buf.len())),
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
    let accent = match app.mode {
        Mode::Insert => t::PINK,
        Mode::Rename => t::PURPLE,
        Mode::Command => t::AMBER,
        _ => t::BORDER,
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
                    Span::styled(r.clone(), Style::default().fg(t::FG)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    r.clone(),
                    Style::default().fg(t::FG),
                )));
            }
        }
        // Empty Insert composer: show a gentle placeholder after the prompt.
        if app.mode == Mode::Insert && text.is_empty() {
            if let Some(first) = lines.first_mut() {
                first.spans.push(Span::styled(
                    "ask anything — / for commands".to_string(),
                    Style::default().fg(t::GUTTER),
                ));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);

        let x = (inner.x + wrapped.cursor_col).min(inner.x + inner.width.saturating_sub(1));
        let y = inner.y + ((wrapped.cursor_row - start) as u16).min(inner.height.saturating_sub(1));
        f.set_cursor_position(Position::new(x, y));
        return;
    }

    // Normal mode: keep the drafted text visible (dimmed) or show the hints.
    let line = if !app.input.is_empty() {
        Line::from(vec![
            Span::styled("❯ ".to_string(), Style::default().fg(t::DIM)),
            Span::styled(
                app.input.replace('\n', " ⏎ "),
                Style::default().fg(t::DIM),
            ),
            Span::styled("  (i to edit)".to_string(), Style::default().fg(t::GUTTER)),
        ])
    } else {
        Line::from(Span::styled(
            " i type · j/k scroll · za tools · r rename · ? keys · q quit",
            Style::default().fg(t::DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), inner);
}

// lualine-style powerline statusline.
fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, mode_col) = match app.mode {
        Mode::Normal => ("NORMAL", t::MODE_NORMAL),
        Mode::Insert => ("INSERT", t::MODE_INSERT),
        Mode::Command => ("COMMAND", t::MODE_COMMAND),
        Mode::Rename => ("RENAME", t::MODE_VISUAL),
        Mode::Confirm => ("CONFIRM", t::RED),
    };
    let c = &app.chat;
    // A toast (bad command, corrupt state file, …) takes the info segment until
    // the next keypress.
    let info = match &app.toast {
        Some(m) => format!(" {} ", truncate_width(m, 60)),
        None => format!(" {} ", app.model_display),
    };
    let info_st = if app.toast.is_some() {
        Style::default().fg(t::AMBER).bg(t::PANEL)
    } else {
        Style::default().fg(t::DIM).bg(t::PANEL)
    };

    let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(20)]).split(area);

    // left: [ mode ][ info ]
    let left = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(t::PANEL)
                .bg(mode_col)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(SEP_R, Style::default().fg(mode_col).bg(t::PANEL)),
        Span::styled(info, info_st),
        Span::styled(SEP_R, Style::default().fg(t::PANEL)),
    ]);
    f.render_widget(Paragraph::new(left), cols[0]);

    // right: turn state + running cost (only once it rounds to a visible cent).
    let cost = if c.cost >= 0.005 {
        format!("${:.2} ", c.cost)
    } else {
        String::new()
    };
    let right = if c.in_flight {
        let secs = c.turn_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let dot = if c.interrupting { "⎋ " } else { "● " };
        Line::from(vec![
            Span::styled(dot, Style::default().fg(t::AMBER)),
            Span::styled(format!("{} ", fmt_secs(secs)), Style::default().fg(t::FG)),
            Span::styled(cost, Style::default().fg(t::DIM)),
        ])
    } else {
        Line::from(vec![
            Span::styled("idle ", Style::default().fg(t::DIM)),
            Span::styled(cost, Style::default().fg(t::DIM)),
        ])
    };
    f.render_widget(Paragraph::new(right).right_aligned(), cols[1]);
}

fn render_confirm(f: &mut Frame, area: Rect, app: &App) {
    let msg = app.confirm_msg.clone();
    let w = (msg.width() as u16 + 6).clamp(24, area.width.saturating_sub(4).max(24));
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
            vec![("z", "help / all keybinds"), ("e", "expand/collapse tools")],
        ),
        Pending::G => ("g", vec![("g", "top of transcript")]),
        Pending::Z => (
            "z",
            vec![("z", "jump to newest"), ("a", "expand/collapse tool output")],
        ),
        Pending::None => return,
    };

    let w = 38u16.min(area.width);
    let h = (entries.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y + area.height.saturating_sub(h + 1).min(area.height.saturating_sub(h)),
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
        ("", "TALK"),
        ("type + Enter", "send (stays in Insert; launch starts here)"),
        ("Shift/Alt-Enter", "newline · Ctrl-j same"),
        ("/", "slash commands — Tab completes, Enter sends"),
        ("/clear · :clear", "wipe transcript, fresh session"),
        ("", "STEER"),
        ("Esc / Ctrl-c", "interrupt the running turn (again = force)"),
        ("(while busy)", "keep typing — sends queue up in order"),
        ("", "READ (Esc → Normal)"),
        ("j / k", "scroll · Ctrl-d/u half page · PgUp/PgDn"),
        ("gg / G", "top / bottom (follow the stream)"),
        ("za", "expand / collapse tool output"),
        ("zz", "jump back to the newest activity"),
        ("", "COMPOSER"),
        ("←→↑↓ Home End", "move cursor · Ctrl-a/e line ends"),
        ("Ctrl-w / Ctrl-u", "delete word / clear"),
        ("paste", "Cmd-V multi-line intact · Ctrl-v = pbpaste"),
        ("", "MISC"),
        ("r", "rename conversation (shown in header)"),
        (":mouse", "wheel-scroll vs cursor-select"),
        ("q", "quit (asks) · :q quits directly"),
        ("?  · Space z", "this help"),
    ];

    let w = 62u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 2).min(area.height.saturating_sub(2).max(3));
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t::PURPLE))
        .title(Span::styled(
            " aeovim — keys  (any key to close) ",
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
                        format!(" {k:>15}  "),
                        Style::default().fg(t::PINK).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(d.to_string(), Style::default().fg(t::FG)),
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
        let l = Line::from(vec![
            Span::styled("red ".to_string(), Style::default().fg(t::RED)),
            Span::styled("bluebluexx".to_string(), Style::default().fg(t::PERI)),
        ]);
        let mut out = Vec::new();
        wrap_line_into(&l, 8, &mut out);
        assert!(out.len() >= 2);
        // first row keeps the red span styled red
        assert_eq!(out[0].spans[0].style.fg, Some(t::RED));
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
}
