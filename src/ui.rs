//! Rendering. Layout: [ sidebar | main( tabline / transcript / composer / status ) ].
//! The sidebar (toggle: `b`) groups chats into Chats / Parallel / Looping / Previous,
//! neo-tree style. Aesthetic will later be tuned to match the user's nvim setup.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Chat, ChatKind, Entry, Mode};

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let side_w = if app.sidebar_open {
        30u16.min(area.width.saturating_sub(20))
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
}

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " agents ",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    push_section(&mut lines, "CHATS");
    push_group(&mut lines, app, Some(ChatKind::Solo), false);
    push_section(&mut lines, "PARALLEL");
    push_group(&mut lines, app, Some(ChatKind::Parallel), false);
    push_section(&mut lines, "LOOPING");
    push_group(&mut lines, app, Some(ChatKind::Loop), false);
    push_section(&mut lines, "PREVIOUS");
    push_group(&mut lines, app, None, true);

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn push_section(lines: &mut Vec<Line>, name: &str) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("▾ {name}"),
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    )));
}

fn push_group(lines: &mut Vec<Line>, app: &App, kind: Option<ChatKind>, closed: bool) {
    let mut any = false;
    for (i, c) in app.chats.iter().enumerate() {
        if c.closed != closed {
            continue;
        }
        if let Some(k) = kind {
            if c.kind != k {
                continue;
            }
        }
        any = true;
        let glyph = if c.in_flight {
            SPIN[app.spinner % SPIN.len()]
        } else if closed {
            "·"
        } else {
            "●"
        };
        let base = if closed {
            Style::default().fg(Color::DarkGray)
        } else if c.in_flight {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray)
        };
        let style = if i == app.active {
            base.add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            base
        };
        lines.push(Line::from(Span::styled(format!("  {glyph} {}", c.title), style)));
    }
    if !any {
        lines.push(Line::from(Span::styled(
            "  —",
            Style::default().fg(Color::DarkGray),
        )));
    }
}

fn render_tabline(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " aeovim ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    for (i, c) in app.chats.iter().enumerate() {
        if c.closed {
            continue;
        }
        let glyph = if c.in_flight {
            SPIN[app.spinner % SPIN.len()]
        } else {
            "●"
        };
        let label = format!(" {}:{} {glyph} ", i + 1, c.title);
        let st = if i == app.active {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
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
    let user_lbl = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let asst_lbl = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let tool_st = Style::default().fg(Color::Yellow);
    let note_st = Style::default().fg(Color::DarkGray);
    let err_st = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::Gray);

    let mut out: Vec<Line> = Vec::new();
    for (i, e) in chat.transcript.iter().enumerate() {
        if i > 0 {
            out.push(Line::from(""));
        }
        match e {
            Entry::User(t) => push_block(&mut out, "you", user_lbl, t, body),
            Entry::Assistant(t) => push_block(&mut out, "claude", asst_lbl, t, body),
            Entry::Tool(n) => out.push(Line::from(Span::styled(format!("  ⚙ {n}"), tool_st))),
            Entry::Note(t) => out.push(Line::from(Span::styled(format!("  {t}"), note_st))),
            Entry::Error(t) => push_block(&mut out, "! error", err_st, t, err_st),
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
    let border = if app.mode == Mode::Insert {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(" prompt ", Style::default().fg(Color::DarkGray)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let content = if app.mode == Mode::Insert {
        Line::from(vec![
            Span::styled("❯ ", Style::default().fg(Color::Cyan)),
            Span::raw(app.input.clone()),
        ])
    } else if app.active_chat().in_flight {
        Line::from(Span::styled(
            " …working — i to compose next, q to quit",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            " press i to compose · Enter sends · b sidebar · q quits",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(content), inner);

    if app.mode == Mode::Insert {
        let x = (inner.x + 2 + app.input.chars().count() as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        f.set_cursor_position(Position::new(x, inner.y));
    }
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, chip) = match app.mode {
        Mode::Normal => (
            " NORMAL ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Mode::Insert => (
            " INSERT ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
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
    let hints = "   b:sidebar n:new Tab:switch x:close i:compose q:quit";
    let line = Line::from(vec![
        Span::styled(label, chip),
        Span::styled(mid, Style::default().fg(Color::DarkGray)),
        Span::styled(
            activity,
            Style::default().fg(if c.in_flight {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
