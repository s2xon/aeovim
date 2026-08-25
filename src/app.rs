//! App state + update logic — single-session model.
//!
//! One launch = one conversation with one long-lived claude child, DeepSeek-TUI
//! style. Multiplexing lives outside (tmux panes each run their own `avim`,
//! keyed per tmux session). The app starts in Insert mode, ready to type.

use std::time::Instant;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use serde_json::Value;

use crate::protocol::AgentEvent;
use crate::store::{self, PersistChat, PersistSpace};

#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Rename,
    Confirm,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Pending {
    None,
    G,
    Z,
    Leader,
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DiffKind {
    Ctx,
    Add,
    Del,
    Gap,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Entry {
    User(String),
    Assistant(String),
    Tool(String),
    ToolResult { ok: bool, text: String },
    /// A code edit rendered as a colored +/- hunk (Claude-Code-style), sitting
    /// directly under its `● Update(file)` tool header.
    Diff {
        file: String,
        added: usize,
        removed: usize,
        lines: Vec<DiffLine>,
    },
    Note(String),
    Error(String),
}

pub struct Chat {
    pub title: String,
    pub transcript: Vec<Entry>,
    /// Bumped on every transcript mutation — the render cache's invalidation key.
    pub rev: u64,
    pub streaming: Option<String>,
    pub in_flight: bool,
    pub session_id: String,
    pub first_turn: bool,
    pub cost: f64,
    pub scroll: usize,
    pub follow: bool,
    pub last_max_scroll: usize,
    /// When the in-flight turn started — drives the live "Ns" elapsed counter.
    pub turn_started: Option<Instant>,
    /// Short label of what the turn is doing right now ("Thinking",
    /// "Bash(cargo build)", "Responding") — shown in the working line.
    pub activity: Option<String>,
    /// Last prompt sent, kept so a stale-session resume can be replayed once.
    pub last_prompt: Option<String>,
    /// Guards the resume self-heal to a single retry per turn.
    pub healed_once: bool,
    /// Prompts typed while a turn is in flight — sent in order as it frees up.
    pub queue: Vec<String>,
    /// The live claude child (spawned lazily on first send).
    pub session: Option<crate::agent::SessionHandle>,
    /// An interrupt was requested for the in-flight turn (Esc again = hard kill).
    pub interrupting: bool,
    /// za — render tool results in full instead of one summary line.
    pub expand_tools: bool,
    /// In-flight tool calls: (tool_use_id, transcript index of the ● Tool entry),
    /// so each result lands under ITS call even when calls run in parallel.
    pub pending_tools: Vec<(String, usize)>,
    /// Pre-wrapped visual rows of the settled transcript, keyed by (rev, width,
    /// expand_tools). Streaming only rebuilds the tail — never this.
    pub cache: crate::ui::RenderCache,
}

impl Chat {
    fn fresh() -> Self {
        let session_id = Uuid::new_v4().to_string();
        Chat::blank(session_id, String::new(), true, 0.0)
    }

    fn from_persist(pc: &PersistChat) -> Self {
        // `started` decides --session-id vs --resume on the next spawn: a chat
        // that never actually sent a turn must NOT try to resume a session that
        // claude has no record of.
        let mut c = Chat::blank(pc.session_id.clone(), pc.title.clone(), !pc.started, pc.cost);
        if !pc.transcript.is_empty() {
            c.transcript = pc.transcript.clone();
            c.rev += 1;
        }
        c
    }

    fn blank(session_id: String, title: String, first_turn: bool, cost: f64) -> Self {
        Chat {
            title,
            transcript: Vec::new(),
            rev: 0,
            streaming: None,
            in_flight: false,
            session_id,
            first_turn,
            cost,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
            turn_started: None,
            activity: None,
            last_prompt: None,
            healed_once: false,
            queue: Vec::new(),
            session: None,
            interrupting: false,
            expand_tools: false,
            pending_tools: Vec::new(),
            cache: crate::ui::RenderCache::default(),
        }
    }

    /// Append an entry. ALL transcript mutations go through push/insert/clear so
    /// `rev` stays honest — the render cache keys off it.
    pub fn push(&mut self, e: Entry) {
        self.transcript.push(e);
        self.rev += 1;
    }

    fn note(&mut self, s: impl Into<String>) {
        self.push(Entry::Note(s.into()));
    }

    /// Insert a tool result directly under its tool call (and its diff, if any),
    /// keeping `pending_tools` indices consistent.
    fn insert_tool_result(&mut self, id: &str, ok: bool, text: String) {
        let entry = Entry::ToolResult { ok, text };
        let Some(pos) = self.pending_tools.iter().position(|(tid, _)| tid == id) else {
            self.push(entry);
            return;
        };
        let (_, tidx) = self.pending_tools.remove(pos);
        let mut at = (tidx + 1).min(self.transcript.len());
        if matches!(self.transcript.get(at), Some(Entry::Diff { .. })) {
            at += 1;
        }
        self.transcript.insert(at, entry);
        self.rev += 1;
        for (_, i) in self.pending_tools.iter_mut() {
            if *i >= at {
                *i += 1;
            }
        }
    }

    fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.pending_tools.clear();
        self.rev += 1;
    }

    fn commit_streaming(&mut self) {
        if let Some(s) = self.streaming.take() {
            if !s.trim().is_empty() {
                self.push(Entry::Assistant(s));
            }
        }
    }

    /// Keep the transcript bounded so it doesn't grow without limit on disk or
    /// in memory. Only called at turn boundaries (pending_tools indices would
    /// otherwise shift mid-turn).
    fn trim(&mut self) {
        const CAP: usize = 300;
        const MARK: &str = "… earlier messages trimmed";
        if self.transcript.len() <= CAP {
            return;
        }
        let drop = self.transcript.len() - CAP;
        self.transcript.drain(0..drop);
        let marked = matches!(self.transcript.first(), Some(Entry::Note(n)) if n == MARK);
        if !marked {
            self.transcript.insert(0, Entry::Note(MARK.into()));
        }
        self.rev += 1;
    }
}

/// Bound one tool result for storage: keep the head and tail, elide the middle.
/// Full output for normal cases, but a 100k-line build log can't bloat the
/// transcript, the render, or the state file.
fn cap_tool_text(text: &str) -> String {
    const HEAD: usize = 160;
    const TAIL: usize = 40;
    const MAX_BYTES: usize = 32 * 1024;
    let lines: Vec<&str> = text.lines().collect();
    let capped: String = if lines.len() > HEAD + TAIL + 1 {
        let omitted = lines.len() - HEAD - TAIL;
        let mut s = lines[..HEAD].join("\n");
        s.push('\n');
        s.push_str(&format!("… {omitted} lines omitted …"));
        s.push('\n');
        s.push_str(&lines[lines.len() - TAIL..].join("\n"));
        s
    } else {
        text.to_string()
    };
    if capped.len() > MAX_BYTES {
        let mut cut = MAX_BYTES;
        while !capped.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (truncated)", &capped[..cut])
    } else {
        capped
    }
}

pub fn chat_title(c: &Chat) -> String {
    if c.title.trim().is_empty() {
        "new conversation".to_string()
    } else {
        c.title.clone()
    }
}

/// True when `name` is still an auto-generated default or empty — used so the
/// first rename starts from an empty buffer, and auto-naming can take over.
fn is_default_name(name: &str) -> bool {
    let name = name.trim();
    name.is_empty() || name == "new conversation"
}

fn slug(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.trim().is_empty() {
        return "conversation".into();
    }
    let mut out: String = one_line.chars().take(36).collect();
    if one_line.chars().count() > 36 {
        out.push('…');
    }
    out
}

/// Byte offset of the grapheme boundary before `at` (0 if already at start).
pub fn prev_grapheme(s: &str, at: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let mut prev = 0;
    for (i, _) in s.grapheme_indices(true) {
        if i >= at {
            break;
        }
        prev = i;
    }
    prev
}

/// Byte offset of the grapheme boundary after `at` (len if already at end).
pub fn next_grapheme(s: &str, at: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    for (i, g) in s.grapheme_indices(true) {
        if i >= at {
            return i + g.len();
        }
    }
    s.len()
}

fn short_path(p: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = format!("{}/", cwd.display());
        if let Some(rel) = p.strip_prefix(&cwd) {
            return rel.to_string();
        }
    }
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// Strip ANSI/OSC escape sequences and other control chars so no raw escapes
/// ever reach the ratatui cell buffer (they'd otherwise be re-interpreted by the
/// terminal and corrupt the screen). Tabs → space.
pub fn clean_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.peek() {
                Some('[') => {
                    it.next();
                    // CSI: consume until a final byte 0x40..=0x7e
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    // OSC: consume until BEL or ESC
                    while let Some(&n) = it.peek() {
                        it.next();
                        if n == '\u{07}' || n == '\u{1b}' {
                            break;
                        }
                    }
                }
                // Two-char escape (ESC c, ESC 7, alt-key…): consume the pair,
                // but never eat a non-ASCII char that just followed a stray ESC.
                Some(&n) if n.is_ascii() => {
                    it.next();
                }
                _ => {}
            },
            '\t' => out.push(' '),
            // Bidi overrides / isolates / marks: a model emitting U+202E would
            // reverse the rest of the on-screen line — drop them.
            '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Clean pasted text for the composer: normalise CRLF/CR → LF, keep newlines,
/// expand tabs to spaces, and drop other control chars (so a paste can't smuggle
/// raw escapes into the buffer). Newlines are preserved for multi-line prompts.
fn sanitize_paste(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            // Strip whole ANSI CSI / OSC sequences (pasted terminal output), not
            // just the ESC byte — otherwise "[31m" litter survives.
            '\u{1b}' => match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if n == '\u{07}' || n == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {
                    it.next();
                }
            },
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn truncate_str(s: &str, n: usize) -> String {
    let one = clean_line(&s.replace('\n', " "));
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

/// Format a tool call the way Claude Code shows it: ● Tool(arg) + a summary.
fn format_tool(name: &str, input: &Value) -> String {
    let get = |k: &str| input.get(k).and_then(|x| x.as_str()).unwrap_or("");
    match name {
        "Edit" | "MultiEdit" | "Update" => {
            let f = short_path(get("file_path"));
            let old = get("old_string");
            let new = get("new_string");
            let a = if new.is_empty() { 0 } else { new.lines().count().max(1) };
            let r = if old.is_empty() { 0 } else { old.lines().count().max(1) };
            let summ = if r == 0 {
                format!("Added {a} lines")
            } else if a == 0 {
                format!("Removed {r} lines")
            } else {
                format!("+{a} -{r}")
            };
            format!("● Update({f})\n{summ}")
        }
        "Write" | "NotebookEdit" => {
            let f = short_path(get("file_path"));
            let n = get("content").lines().count().max(1);
            format!("● Write({f})\nAdded {n} lines")
        }
        "Read" => format!("● Read({})", short_path(get("file_path"))),
        "Bash" => format!("● Bash({})", truncate_str(get("command"), 64)),
        "Grep" => format!("● Grep({})", truncate_str(get("pattern"), 48)),
        "Glob" => format!("● Glob({})", truncate_str(get("pattern"), 48)),
        "WebSearch" => format!("● Search({})", truncate_str(get("query"), 48)),
        "WebFetch" => format!("● Fetch({})", truncate_str(get("url"), 56)),
        "Task" => format!("● Task({})", truncate_str(get("description"), 48)),
        "TodoWrite" => format_todos(input),
        other => format!("● {other}"),
    }
}

/// Line-level diff of `old` → `new` via LCS, then collapse long runs of
/// unchanged context to a few lines around each change (Gap marks the elision).
/// Returns (added, removed, lines). Guards against pathological sizes.
fn line_diff(old: &str, new: &str) -> (usize, usize, Vec<DiffLine>) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mk = |kind, s: &str| DiffLine { kind, text: clean_line(s) };

    // Big replacements: skip the O(n·m) table and just show removals then adds.
    if a.len() > 400 || b.len() > 400 {
        let mut lines: Vec<DiffLine> = a.iter().map(|l| mk(DiffKind::Del, l)).collect();
        lines.extend(b.iter().map(|l| mk(DiffKind::Add, l)));
        return (b.len(), a.len(), collapse_ctx(lines));
    }

    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut added, mut removed) = (0, 0);
    let mut lines = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            lines.push(mk(DiffKind::Ctx, a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            lines.push(mk(DiffKind::Del, a[i]));
            removed += 1;
            i += 1;
        } else {
            lines.push(mk(DiffKind::Add, b[j]));
            added += 1;
            j += 1;
        }
    }
    while i < n {
        lines.push(mk(DiffKind::Del, a[i]));
        removed += 1;
        i += 1;
    }
    while j < m {
        lines.push(mk(DiffKind::Add, b[j]));
        added += 1;
        j += 1;
    }
    (added, removed, collapse_ctx(lines))
}

/// Keep at most `CTX` context lines adjacent to any change; replace longer
/// unchanged stretches with a single Gap marker so the hunk stays compact.
fn collapse_ctx(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    const CTX: usize = 2;
    let n = lines.len();
    let changed: Vec<bool> = lines.iter().map(|l| l.kind != DiffKind::Ctx).collect();
    // A context line is kept if some change lies within CTX rows of it.
    let keep: Vec<bool> = (0..n)
        .map(|idx| {
            let lo = idx.saturating_sub(CTX);
            let hi = (idx + CTX + 1).min(n);
            lines[idx].kind != DiffKind::Ctx || changed[lo..hi].iter().any(|&c| c)
        })
        .collect();
    let mut out = Vec::new();
    let mut gapped = false;
    for idx in 0..n {
        if keep[idx] {
            out.push(lines[idx].clone());
            gapped = false;
        } else if !gapped {
            out.push(DiffLine {
                kind: DiffKind::Gap,
                text: "⋯".into(),
            });
            gapped = true;
        }
    }
    out
}

/// Bound a rendered hunk: a Write of a 5000-line file must not become 5000
/// transcript lines (rendered every frame, persisted every save). Head + tail
/// with an elision marker keeps the shape visible.
fn cap_diff(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    const HEAD: usize = 120;
    const TAIL: usize = 30;
    if lines.len() <= HEAD + TAIL + 1 {
        return lines;
    }
    let omitted = lines.len() - HEAD - TAIL;
    let mut out: Vec<DiffLine> = lines[..HEAD].to_vec();
    out.push(DiffLine {
        kind: DiffKind::Gap,
        text: format!("⋯ {omitted} more lines"),
    });
    out.extend_from_slice(&lines[lines.len() - TAIL..]);
    out
}

/// Build a diff entry for an edit-shaped tool (Edit/MultiEdit/Write/…). Returns
/// (file, added, removed, lines), or None for tools that aren't edits.
fn build_diff(name: &str, input: &Value) -> Option<(String, usize, usize, Vec<DiffLine>)> {
    let get = |k: &str| input.get(k).and_then(|x| x.as_str()).unwrap_or("");
    match name {
        "Edit" | "Update" => {
            let (a, r, lines) = line_diff(get("old_string"), get("new_string"));
            Some((short_path(get("file_path")), a, r, cap_diff(lines)))
        }
        "Write" => {
            let (a, r, lines) = line_diff("", get("content"));
            Some((short_path(get("file_path")), a, r, cap_diff(lines)))
        }
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array)?;
            let (mut added, mut removed) = (0, 0);
            let mut lines: Vec<DiffLine> = Vec::new();
            for (k, e) in edits.iter().enumerate() {
                let os = e.get("old_string").and_then(Value::as_str).unwrap_or("");
                let ns = e.get("new_string").and_then(Value::as_str).unwrap_or("");
                let (a, r, mut ls) = line_diff(os, ns);
                added += a;
                removed += r;
                if k > 0 {
                    lines.push(DiffLine { kind: DiffKind::Gap, text: "⋯".into() });
                }
                lines.append(&mut ls);
            }
            Some((short_path(get("file_path")), added, removed, cap_diff(lines)))
        }
        _ => None,
    }
}

/// One-line label of a running tool for the working line, e.g. "Bash(cargo
/// build)" — the first line of `format_tool` with its leading "● " stripped.
fn tool_activity(name: &str, input: &Value) -> String {
    let full = format_tool(name, input);
    let first = full.lines().next().unwrap_or(name);
    first.trim_start_matches("● ").to_string()
}

fn format_todos(input: &Value) -> String {
    let mut out = String::from("● Todos");
    if let Some(arr) = input.get("todos").and_then(Value::as_array) {
        for t in arr {
            let content = t.get("content").and_then(Value::as_str).unwrap_or("");
            let mark = match t.get("status").and_then(Value::as_str) {
                Some("completed") => "✔",
                Some("in_progress") => "▶",
                _ => "☐",
            };
            out.push_str(&format!("\n  {mark} {content}"));
        }
    }
    out
}

pub enum Msg {
    Input(Event),
    Tick,
    // `chat` ids stay on the wire so agent.rs needs no rewiring when
    // multi-session returns; the single-session app ignores them.
    #[allow(dead_code)]
    Agent { chat: u64, ev: AgentEvent },
    /// The claude child exited (crash, error, or deliberate kill).
    #[allow(dead_code)]
    SessionEnded { chat: u64, error: Option<String> },
    /// Clipboard contents read off the UI task (Ctrl-v fallback).
    Pasted(String),
}

/// The single chat's id on the Msg wire (kept so agent.rs stays id-based and a
/// multi-session future doesn't rewire the protocol).
const CHAT_ID: u64 = 1;

pub struct App {
    pub mode: Mode,
    pub pending: Pending,
    pub input: String,
    /// Cursor position in `input`: a byte offset on a grapheme boundary.
    pub input_cursor: usize,
    pub cmd: String,
    pub rename_buf: String,
    pub chat: Chat,
    /// The confirm overlay is asking about quitting.
    pending_quit: bool,
    pub confirm_msg: String,
    /// Something on screen changed since the last draw (main loop's redraw gate).
    pub dirty: bool,
    /// One-line notice for the statusline (corrupt state file, bad command, …).
    pub toast: Option<String>,
    pub model_cli: Option<String>,
    pub model_display: String,
    pub dangerous: bool,
    pub should_quit: bool,
    pub spinner: usize,
    pub help_open: bool,
    /// Mouse capture state. Default OFF so the cursor can select/copy text (and
    /// tmux/Ghostty native selection works); `:mouse` toggles wheel-scroll on.
    pub mouse_capture: bool,
    pub slash_commands: Vec<String>,
    pub slash_sel: usize,
    workspace_key: String,
    tx: UnboundedSender<Msg>,
}

impl App {
    pub fn new(
        model_cli: Option<String>,
        dangerous: bool,
        tx: UnboundedSender<Msg>,
        workspace_key: String,
        restored: Vec<PersistSpace>,
    ) -> Self {
        let model_display = model_cli.clone().unwrap_or_else(|| "default".into());
        // Single-session model: adopt the first saved chat; anything beyond it
        // was already backed up by main() before we overwrite on save.
        let chat = restored
            .first()
            .and_then(|sp| sp.chats.first())
            .map(Chat::from_persist)
            .unwrap_or_else(Chat::fresh);

        Self {
            mode: Mode::Insert, // launch ready to type
            pending: Pending::None,
            input: String::new(),
            input_cursor: 0,
            cmd: String::new(),
            rename_buf: String::new(),
            chat,
            pending_quit: false,
            confirm_msg: String::new(),
            dirty: true,
            toast: None,
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            help_open: false,
            mouse_capture: false,
            slash_commands: Vec::new(),
            slash_sel: 0,
            workspace_key,
            tx,
        }
    }

    /// Flip mouse capture. OFF lets the cursor select/copy text (tmux/Ghostty
    /// native selection); ON restores wheel-scroll but intercepts drag-select.
    fn toggle_mouse(&mut self) {
        self.set_mouse(!self.mouse_capture);
    }

    pub fn set_mouse(&mut self, on: bool) {
        self.mouse_capture = on;
        let mut out = std::io::stdout();
        let _ = if on {
            execute!(out, EnableMouseCapture)
        } else {
            execute!(out, DisableMouseCapture)
        };
        self.toast = Some(
            if on {
                "mouse ON — wheel scrolls; drag-select needs Shift/Option"
            } else {
                "mouse OFF — drag to select/copy; scroll with j/k · Ctrl-d/u"
            }
            .into(),
        );
    }

    pub fn persist(&self) {
        let data = vec![PersistSpace {
            name: String::new(),
            chats: vec![PersistChat {
                title: self.chat.title.clone(),
                session_id: self.chat.session_id.clone(),
                cost: self.chat.cost,
                started: !self.chat.first_turn,
                transcript: self.chat.transcript.clone(),
            }],
        }];
        store::save(&self.workspace_key, &data);
    }

    // ---- events ----

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => {
                // Spinner + elapsed timers advance on the tick, not per event.
                self.spinner = self.spinner.wrapping_add(1);
                return; // tick alone doesn't set dirty; main loop redraws while in-flight
            }
            Msg::Input(Event::Key(k)) => self.handle_key(k),
            Msg::Input(Event::Mouse(m)) => self.handle_mouse(m),
            Msg::Input(Event::Paste(s)) => self.paste(s),
            Msg::Input(_) => {}
            Msg::Pasted(s) => self.paste(s),
            Msg::Agent { ev, .. } => self.handle_agent(ev),
            Msg::SessionEnded { error, .. } => self.session_ended(error),
        }
        self.dirty = true;
    }

    /// The claude child exited. A clean exit (deliberate kill, stdin close) is
    /// quiet; a crash mid-turn surfaces the error. A stale `--resume`
    /// self-heals once by re-minting the session and replaying the last prompt.
    fn session_ended(&mut self, error: Option<String>) {
        if error
            .as_deref()
            .is_some_and(|e| e.contains("No conversation found with session ID"))
        {
            let replay = {
                let c = &mut self.chat;
                c.session = None;
                if !c.healed_once && c.last_prompt.is_some() {
                    c.healed_once = true;
                    c.in_flight = false;
                    c.streaming = None;
                    // Fresh id + first-turn so claude creates the session
                    // cleanly (the old id can't be reused).
                    c.session_id = Uuid::new_v4().to_string();
                    c.first_turn = true;
                    c.note("session was gone — starting fresh");
                    c.last_prompt.clone()
                } else {
                    None
                }
            };
            if let Some(p) = replay {
                self.deliver(p, false);
                return;
            }
        }
        let c = &mut self.chat;
        c.session = None;
        let was_interrupting = c.interrupting;
        c.interrupting = false;
        if c.in_flight {
            c.commit_streaming();
            c.in_flight = false;
            if was_interrupting {
                c.note("⎋ stopped");
            } else if error.is_none() {
                c.note("session ended");
            }
        }
        c.turn_started = None;
        c.activity = None;
        c.pending_tools.clear();
        if let Some(e) = error {
            c.push(Entry::Error(e));
        }
        c.trim();
        self.persist();
        self.drain_queue();
    }

    fn handle_agent(&mut self, ev: AgentEvent) {
        let is_result = matches!(ev, AgentEvent::TurnResult { .. });
        let c = &mut self.chat;
        match ev {
            AgentEvent::Init { session_id, model, slash_commands } => {
                if let Some(s) = session_id {
                    c.session_id = s;
                }
                if let Some(m) = model {
                    self.model_display = m;
                }
                if !slash_commands.is_empty() {
                    self.slash_commands = slash_commands;
                }
            }
            AgentEvent::TextDelta(s) => {
                // NOTE: never touches `follow` — content arrival must not
                // fight the user's scroll position.
                c.activity = Some("Responding".into());
                c.streaming.get_or_insert_with(String::new).push_str(&s);
            }
            AgentEvent::ThinkingDelta => {
                if c.streaming.is_none() {
                    c.activity = Some("Thinking".into());
                }
            }
            AgentEvent::AssistantText(s) => {
                // Authoritative text of one assistant message: replaces the
                // delta accumulation (which can be lossy) and commits, so a
                // multi-message turn (text → tools → text) keeps its shape.
                c.streaming = None;
                c.push(Entry::Assistant(s));
            }
            AgentEvent::ToolCall { id, name, input } => {
                c.commit_streaming();
                c.activity = Some(tool_activity(&name, &input));
                // Edits render as a colored +/- hunk under a bare header; every
                // other tool keeps its one-line `● Tool(arg)` summary.
                let tool_idx = c.transcript.len();
                if let Some((file, added, removed, lines)) = build_diff(&name, &input) {
                    let verb = if name == "Write" { "Write" } else { "Update" };
                    c.push(Entry::Tool(format!("● {verb}({file})")));
                    c.push(Entry::Diff { file, added, removed, lines });
                } else {
                    c.push(Entry::Tool(format_tool(&name, &input)));
                }
                if !id.is_empty() {
                    c.pending_tools.push((id, tool_idx));
                }
            }
            AgentEvent::ToolResult { id, ok, text } => {
                // Back to thinking — the model decides its next step.
                c.activity = Some("Thinking".into());
                c.insert_tool_result(&id, ok, cap_tool_text(&text));
            }
            AgentEvent::TurnResult { cost_usd, is_error, subtype, .. } => {
                // NOTE: result.text is deliberately unused — every assistant
                // message already arrived via AssistantText; re-adding the
                // result text duplicated the final message.
                c.commit_streaming();
                c.cost += cost_usd;
                c.in_flight = false;
                c.turn_started = None;
                c.activity = None;
                c.pending_tools.clear();
                if c.interrupting {
                    // We asked for this stop — a status line, not an error.
                    c.interrupting = false;
                    c.note("⎋ interrupted");
                } else if is_error {
                    let what = if subtype.is_empty() {
                        "turn ended with error".to_string()
                    } else {
                        format!("turn ended with error ({subtype})")
                    };
                    c.push(Entry::Error(what));
                }
                c.trim();
            }
            AgentEvent::ControlDone { ok } => {
                // Interrupt acked; the turn's error_during_execution result
                // follows and lands in the arm above. A refused interrupt
                // is worth surfacing (Esc again will force-kill).
                if !ok && c.interrupting {
                    c.note("interrupt not acknowledged — Esc again to force-kill");
                }
            }
        }
        if is_result {
            self.persist();
            self.drain_queue();
        }
    }

    // ---- input ----

    fn handle_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.toast = None; // any keypress clears the statusline notice
        let close_key = matches!(
            k.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ')
        );
        if self.help_open {
            self.help_open = false;
            if close_key {
                return;
            }
            // other keys close the overlay AND still do their thing
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Command => self.key_command(k, ctrl),
            Mode::Rename => self.key_rename(k, ctrl),
            Mode::Insert => self.key_insert(k, ctrl),
            Mode::Confirm => self.key_confirm(k),
            Mode::Normal => {
                if self.pending != Pending::None {
                    self.handle_pending(k);
                    return;
                }
                self.key_normal(k, ctrl);
            }
        }
    }

    fn key_command(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => {
                self.cmd.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.exec_command(),
            KeyCode::Backspace => {
                let at = self.cmd.len();
                let start = prev_grapheme(&self.cmd, at);
                self.cmd.truncate(start);
            }
            KeyCode::Char('c') if ctrl => {
                self.cmd.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Char('u') if ctrl => self.cmd.clear(),
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Char(c) => self.cmd.push(c),
            _ => {}
        }
    }

    fn key_rename(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let name = self.rename_buf.trim().to_string();
                if !name.is_empty() {
                    self.chat.title = name;
                }
                self.mode = Mode::Normal;
                self.persist();
            }
            KeyCode::Backspace => {
                let at = self.rename_buf.len();
                let start = prev_grapheme(&self.rename_buf, at);
                self.rename_buf.truncate(start);
            }
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Char('u') if ctrl => self.rename_buf.clear(),
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Char(c) => self.rename_buf.push(c),
            _ => {}
        }
    }

    fn key_insert(&mut self, k: KeyEvent, ctrl: bool) {
        // slash-command popup navigation (when typing "/..." with no space yet).
        // Tab completes the selection; Enter always SENDS what's typed.
        if self.slash_active() {
            match k.code {
                KeyCode::Down => {
                    self.slash_move(1);
                    return;
                }
                KeyCode::Up => {
                    self.slash_move(-1);
                    return;
                }
                KeyCode::Char('n') | KeyCode::Char('j') if ctrl => {
                    self.slash_move(1);
                    return;
                }
                KeyCode::Char('p') | KeyCode::Char('k') if ctrl => {
                    self.slash_move(-1);
                    return;
                }
                KeyCode::Tab => {
                    self.slash_complete();
                    return;
                }
                _ => {}
            }
        }
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            // Shift/Alt+Enter inserts a newline (the box grows); plain Enter sends.
            KeyCode::Enter
                if k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_at_cursor("\n");
            }
            KeyCode::Char('j') if ctrl => self.insert_at_cursor("\n"),
            KeyCode::Enter => self.send_prompt(),
            KeyCode::Backspace => self.backspace_at_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Left => self.cursor_left(),
            KeyCode::Right => self.cursor_right(),
            KeyCode::Up => self.cursor_vertical(-1),
            KeyCode::Down => self.cursor_vertical(1),
            KeyCode::Home => self.cursor_line_home(),
            KeyCode::End => self.cursor_line_end(),
            KeyCode::Char('a') if ctrl => self.cursor_line_home(),
            KeyCode::Char('e') if ctrl => self.cursor_line_end(),
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.input_cursor = 0;
                self.slash_sel = 0;
            }
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Char('d') if ctrl => self.scroll_down(8),
            // Ctrl-C never quits from Insert: it clears the draft, or interrupts
            // the running turn, or drops to Normal — in that order.
            KeyCode::Char('c') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.slash_sel = 0;
                } else if self.chat.in_flight {
                    self.interrupt();
                } else {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::PageUp => self.scroll_up(12),
            KeyCode::PageDown => self.scroll_down(12),
            KeyCode::Char(c) => {
                let mut buf = [0u8; 4];
                self.insert_at_cursor(c.encode_utf8(&mut buf));
            }
            _ => {}
        }
    }

    // ---- composer editing (cursor is a byte offset, always on a grapheme
    // boundary — moves and deletes operate on whole graphemes so emoji and
    // combining accents never shatter) ----

    fn insert_at_cursor(&mut self, s: &str) {
        let at = self.input_cursor.min(self.input.len());
        self.input.insert_str(at, s);
        self.input_cursor = at + s.len();
        self.slash_sel = 0;
    }

    fn backspace_at_cursor(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        if at == 0 {
            return;
        }
        let start = prev_grapheme(&self.input, at);
        self.input.replace_range(start..at, "");
        self.input_cursor = start;
        self.slash_sel = 0;
    }

    fn delete_at_cursor(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        if at >= self.input.len() {
            return;
        }
        let end = next_grapheme(&self.input, at);
        self.input.replace_range(at..end, "");
        self.slash_sel = 0;
    }

    fn cursor_left(&mut self) {
        self.input_cursor = prev_grapheme(&self.input, self.input_cursor.min(self.input.len()));
    }

    fn cursor_right(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        if at < self.input.len() {
            self.input_cursor = next_grapheme(&self.input, at);
        }
    }

    fn cursor_line_home(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        self.input_cursor = self.input[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    }

    fn cursor_line_end(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        self.input_cursor = self.input[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.input.len());
    }

    /// Move up/down one logical line, keeping the char column when possible.
    fn cursor_vertical(&mut self, dir: isize) {
        let at = self.input_cursor.min(self.input.len());
        let line_start = self.input[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = self.input[line_start..at].chars().count();
        let target_start = if dir < 0 {
            if line_start == 0 {
                return;
            }
            self.input[..line_start - 1]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            let Some(nl) = self.input[at..].find('\n') else {
                return;
            };
            at + nl + 1
        };
        let target_line: &str = self.input[target_start..]
            .split('\n')
            .next()
            .unwrap_or("");
        let mut b = target_start;
        for (n, ch) in target_line.chars().enumerate() {
            if n >= col {
                break;
            }
            b += ch.len_utf8();
        }
        self.input_cursor = b;
    }

    fn delete_word_back(&mut self) {
        let at = self.input_cursor.min(self.input.len());
        let before = &self.input[..at];
        let trimmed = before.trim_end();
        let cut = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input.replace_range(cut..at, "");
        self.input_cursor = cut;
        self.slash_sel = 0;
    }

    /// Ask before quitting — `q` used to kill the app (and the running turn)
    /// with zero friction, which made stray Normal-mode typing catastrophic.
    fn confirm_quit(&mut self) {
        self.confirm_msg = if self.chat.in_flight {
            "quit aeovim? a turn is still running   y / n".to_string()
        } else {
            "quit aeovim?   y / n".to_string()
        };
        self.pending_quit = true;
        self.mode = Mode::Confirm;
    }

    fn all_slash_commands(&self) -> Vec<String> {
        const DEFAULT: &[&str] = &[
            "init", "review", "security-review", "pr-comments", "compact", "context",
            "cost", "agents", "mcp", "memory", "model", "todos", "help", "clear",
        ];
        let mut v: Vec<String> = DEFAULT.iter().map(|s| s.to_string()).collect();
        for c in &self.slash_commands {
            if !v.iter().any(|x| x == c) {
                v.push(c.clone());
            }
        }
        v.sort();
        v.dedup();
        v
    }

    pub fn slash_matches(&self) -> Vec<String> {
        let q = self.input.trim_start_matches('/').to_lowercase();
        self.all_slash_commands()
            .into_iter()
            .filter(|c| q.is_empty() || c.to_lowercase().contains(&q))
            .collect()
    }

    pub fn slash_active(&self) -> bool {
        self.mode == Mode::Insert
            && self.input.starts_with('/')
            && !self.input.contains(char::is_whitespace)
            && !self.slash_matches().is_empty()
    }

    fn slash_move(&mut self, d: isize) {
        let n = self.slash_matches().len();
        if n == 0 {
            return;
        }
        let cur = self.slash_sel.min(n - 1) as isize;
        self.slash_sel = (cur + d).rem_euclid(n as isize) as usize;
    }

    fn slash_complete(&mut self) {
        let matches = self.slash_matches();
        let sel = self.slash_sel.min(matches.len().saturating_sub(1));
        if let Some(cmd) = matches.get(sel) {
            self.input = format!("/{cmd} ");
            self.input_cursor = self.input.len();
        }
        self.slash_sel = 0;
    }

    fn key_confirm(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if self.pending_quit {
                    self.pending_quit = false;
                    self.should_quit = true;
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_quit = false;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn key_normal(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            // Esc/Ctrl-C while a turn runs = interrupt (Claude Code muscle
            // memory). Ctrl-C when idle asks to quit.
            KeyCode::Esc => {
                if self.chat.in_flight {
                    self.interrupt();
                }
            }
            KeyCode::Char('c') if ctrl => {
                if self.chat.in_flight {
                    self.interrupt();
                } else {
                    self.confirm_quit();
                }
            }
            KeyCode::Char('d') if ctrl => self.scroll_down(8),
            KeyCode::Char('u') if ctrl => self.scroll_up(8),
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Char(' ') => self.pending = Pending::Leader,
            KeyCode::Char(':') => {
                self.cmd.clear();
                self.mode = Mode::Command;
            }
            KeyCode::Char('i') | KeyCode::Char('a') => {
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                self.chat.follow = true;
                self.mode = Mode::Insert;
            }
            KeyCode::Char('q') => self.confirm_quit(),
            KeyCode::Char('r') => {
                self.rename_buf = if is_default_name(&self.chat.title) {
                    String::new()
                } else {
                    self.chat.title.clone()
                };
                self.mode = Mode::Rename;
            }
            KeyCode::Char('?') => self.help_open = true,
            KeyCode::Char('g') => self.pending = Pending::G,
            KeyCode::Char('z') => self.pending = Pending::Z,
            KeyCode::Char('G') => self.chat.follow = true,
            KeyCode::Char('}') => self.scroll_down(10),
            KeyCode::Char('{') => self.scroll_up(10),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_up(1),
            KeyCode::PageDown => self.scroll_down(12),
            KeyCode::PageUp => self.scroll_up(12),
            _ => {}
        }
    }

    fn handle_pending(&mut self, k: KeyEvent) {
        match self.pending {
            Pending::G => {
                self.pending = Pending::None;
                if let KeyCode::Char('g') = k.code {
                    self.chat.follow = false;
                    self.chat.scroll = 0;
                }
            }
            Pending::Z => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('z') => {
                        // recenter on the newest activity (the working line)
                        self.chat.follow = true;
                    }
                    KeyCode::Char('a') => {
                        // za — vim fold toggle: expand/collapse tool results
                        self.chat.expand_tools = !self.chat.expand_tools;
                    }
                    _ => {}
                }
            }
            Pending::Leader => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('z') => self.help_open = true, // Space z(z)
                    KeyCode::Char('e') => self.chat.expand_tools = !self.chat.expand_tools,
                    _ => {}
                }
            }
            Pending::None => {}
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let c = &mut self.chat;
        if c.follow {
            return; // already pinned to the live tail
        }
        c.scroll = c.scroll.saturating_add(n).min(c.last_max_scroll);
        // Scrolling back down to the bottom re-engages follow (sticky tail).
        if c.scroll >= c.last_max_scroll {
            c.follow = true;
        }
    }
    fn scroll_up(&mut self, n: usize) {
        let c = &mut self.chat;
        if c.follow {
            c.scroll = c.last_max_scroll;
            c.follow = false;
        }
        c.scroll = c.scroll.saturating_sub(n);
    }

    // ---- mouse ----

    fn handle_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_down(3),
            MouseEventKind::ScrollUp => self.scroll_up(3),
            _ => {}
        }
    }

    fn exec_command(&mut self) {
        let cmd = self.cmd.trim().to_string();
        self.cmd.clear();
        self.mode = Mode::Normal;
        match cmd.as_str() {
            "q" | "q!" | "qa" | "qa!" | "quit" => {
                // Explicit :q is deliberate — no extra confirm.
                self.should_quit = true;
            }
            "w" | "ws" | "write" => self.persist(),
            "clear" => self.reset_conversation(),
            "help" => self.help_open = true,
            "rename" => {
                self.rename_buf = if is_default_name(&self.chat.title) {
                    String::new()
                } else {
                    self.chat.title.clone()
                };
                self.mode = Mode::Rename;
            }
            "mouse" | "mouse toggle" => self.toggle_mouse(),
            "mouse on" => self.set_mouse(true),
            "mouse off" => self.set_mouse(false),
            "" => {}
            // Unknown commands used to vanish silently (:wq, :help → nothing).
            other => self.toast = Some(format!("not a command: :{other}")),
        }
    }

    fn env_prompt(&self) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        format!(
            "You are running inside aeovim — a keyboard-driven terminal UI that wraps the \
Claude Code CLI. Working directory: {cwd}. You're in a terminal on macOS \
(tmux/Ghostty) — keep output concise and terminal-friendly."
        )
    }

    /// Make sure the chat has a live session child, spawning one if needed.
    /// Returns false if the spawn failed (an Error entry is pushed).
    fn ensure_session(&mut self) -> bool {
        if self.chat.session.as_ref().is_some_and(|s| s.is_alive()) {
            return true;
        }
        let spec = crate::agent::SessionSpec {
            chat: CHAT_ID,
            session_id: self.chat.session_id.clone(),
            resume: !self.chat.first_turn,
            model: self.model_cli.clone(),
            dangerous: self.dangerous,
            permission_mode: "acceptEdits".into(),
            append_system_prompt: Some(self.env_prompt()),
        };
        match crate::agent::spawn_session(spec, self.tx.clone()) {
            Ok(h) => {
                self.chat.session = Some(h);
                self.chat.first_turn = false;
                true
            }
            Err(e) => {
                self.chat.push(Entry::Error(e.to_string()));
                false
            }
        }
    }

    /// Deliver a prompt as a turn on the live session. Assumes the user entry
    /// (if any) is already on the transcript — the resume self-heal replays
    /// through here without re-pushing it.
    fn deliver(&mut self, prompt: String, snap_to_bottom: bool) {
        if !self.ensure_session() {
            return;
        }
        let c = &mut self.chat;
        let sent = c
            .session
            .as_ref()
            .map(|s| s.send_user_turn(&prompt))
            .unwrap_or(false);
        if !sent {
            c.session = None;
            c.push(Entry::Error("session is gone — try again".into()));
            return;
        }
        c.streaming = None;
        c.in_flight = true;
        c.interrupting = false;
        if snap_to_bottom {
            c.follow = true; // sending is a user action; jump to the live tail
        }
        c.turn_started = Some(Instant::now());
        c.activity = Some("Thinking".into());
    }

    /// Interrupt the in-flight turn. First press asks the child nicely (control
    /// protocol — keeps the session alive); a second press while still
    /// interrupting hard-kills the child.
    fn interrupt(&mut self) {
        let c = &mut self.chat;
        if !c.in_flight {
            return;
        }
        if c.interrupting {
            if let Some(s) = &c.session {
                s.kill();
            }
            return;
        }
        let asked = c.session.as_ref().map(|s| s.send_interrupt()).unwrap_or(false);
        if asked {
            c.interrupting = true;
            c.activity = Some("Interrupting… (Esc again to force)".into());
        } else if let Some(s) = &c.session {
            s.kill();
        } else {
            // No live child (shouldn't happen while in_flight) — clear the state.
            c.in_flight = false;
            c.turn_started = None;
            c.activity = None;
        }
    }

    /// Stop the live child (used on quit so nothing runs on invisibly).
    pub fn kill_all_sessions(&mut self) {
        if let Some(s) = self.chat.session.take() {
            s.kill();
        }
    }

    /// /clear · :clear — wipe the transcript and start a fresh claude session.
    fn reset_conversation(&mut self) {
        let c = &mut self.chat;
        if let Some(s) = c.session.take() {
            s.kill();
        }
        c.clear_transcript();
        c.note("cleared — fresh session");
        c.streaming = None;
        c.in_flight = false;
        c.interrupting = false;
        c.turn_started = None;
        c.activity = None;
        c.queue.clear();
        c.cost = 0.0;
        c.first_turn = true;
        c.session_id = Uuid::new_v4().to_string();
        self.persist();
    }

    /// Insert pasted text into whatever's being edited. In the composer the
    /// newlines are preserved (the box grows); single-line fields flatten
    /// newlines to spaces. A paste in Normal mode drops into the composer so
    /// "I pasted" just works.
    fn paste(&mut self, raw: String) {
        let text = sanitize_paste(&raw);
        if text.is_empty() {
            return;
        }
        let flat = || text.replace('\n', " ");
        match self.mode {
            Mode::Insert => self.insert_at_cursor(&text),
            Mode::Command => self.cmd.push_str(&flat()),
            Mode::Rename => self.rename_buf.push_str(&flat()),
            Mode::Normal => {
                self.mode = Mode::Insert;
                self.chat.follow = true;
                self.insert_at_cursor(&text);
            }
            Mode::Confirm => {}
        }
    }

    /// Explicit clipboard paste (Ctrl-v) — reads `pbpaste` for terminals/tmux
    /// setups that don't forward bracketed paste. Runs on its own thread so a
    /// slow clipboard can't freeze the UI; the text arrives as `Msg::Pasted`.
    fn paste_from_clipboard(&mut self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(out) = std::process::Command::new("pbpaste").output() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    if !s.is_empty() {
                        let _ = tx.send(Msg::Pasted(s));
                    }
                }
            }
        });
    }

    /// Send the composed prompt. Stays in Insert mode — the natural next action
    /// after sending is typing the follow-up.
    fn send_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        if prompt == "/clear" {
            self.input.clear();
            self.input_cursor = 0;
            self.reset_conversation();
            return;
        }
        self.input.clear();
        self.input_cursor = 0;
        self.slash_sel = 0;
        // Busy? Queue it (shown at the bottom) and send it when the turn frees
        // up — turns on one session are strictly sequential.
        if self.chat.in_flight {
            self.chat.queue.push(prompt);
            self.chat.follow = true;
            return;
        }
        {
            let c = &mut self.chat;
            c.push(Entry::User(prompt.clone()));
            c.last_prompt = Some(prompt.clone());
            c.healed_once = false;
            // Name the conversation off its first prompt.
            if is_default_name(&c.title) {
                c.title = slug(&prompt);
            }
        }
        self.deliver(prompt, true);
    }

    /// A turn just ended — if prompts were queued while it ran, send the next
    /// one. Runs one at a time; the following turn's end drains the rest.
    fn drain_queue(&mut self) {
        if self.chat.in_flight || self.chat.queue.is_empty() {
            return;
        }
        let next = self.chat.queue.remove(0);
        {
            let c = &mut self.chat;
            c.push(Entry::User(next.clone()));
            c.last_prompt = Some(next.clone());
            c.healed_once = false;
        }
        self.deliver(next, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|l| {
                let m = match l.kind {
                    DiffKind::Add => '+',
                    DiffKind::Del => '-',
                    DiffKind::Gap => '~',
                    DiffKind::Ctx => ' ',
                };
                format!("{m}{}", l.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn diff_changes_one_line() {
        let (added, removed, lines) = line_diff("a\nb\nc", "a\nB\nc");
        assert_eq!((added, removed), (1, 1));
        // context a/c kept, b→B shown as -/+
        assert_eq!(render(&lines), " a\n-b\n+B\n c");
    }

    #[test]
    fn diff_pure_insert_from_write() {
        let (added, removed, lines) = line_diff("", "one\ntwo");
        assert_eq!((added, removed), (2, 0));
        assert!(lines.iter().all(|l| l.kind == DiffKind::Add));
    }

    #[test]
    fn collapse_inserts_gap_for_long_context() {
        // 10 unchanged lines then a change — the middle context collapses to a gap.
        let old: String = (0..10).map(|i| format!("l{i}\n")).collect::<String>() + "x";
        let new: String = (0..10).map(|i| format!("l{i}\n")).collect::<String>() + "y";
        let (_, _, lines) = line_diff(&old, &new);
        assert!(lines.iter().any(|l| l.kind == DiffKind::Gap));
        // the change survives the collapse
        assert!(lines.iter().any(|l| l.kind == DiffKind::Del && l.text == "x"));
        assert!(lines.iter().any(|l| l.kind == DiffKind::Add && l.text == "y"));
    }

    #[test]
    fn paste_keeps_newlines_drops_escapes() {
        // CRLF → LF, tab → spaces, embedded ESC/control dropped, newlines kept.
        let out = sanitize_paste("a\r\nb\tc\x1b[31m\nd");
        assert_eq!(out, "a\nb    c\nd");
        // a multi-line paste stays multi-line (would previously send on line 1)
        assert_eq!(sanitize_paste("one\ntwo\nthree").lines().count(), 3);
    }

    #[test]
    fn tool_results_land_under_their_calls() {
        // Two parallel tool calls; results arrive OUT of order — each must
        // still render under its own ● header (the old code appended blindly).
        let mut c = Chat::blank("sid".into(), "t".into(), true, 0.0);
        c.push(Entry::User("go".into()));
        let a_idx = c.transcript.len();
        c.push(Entry::Tool("● Bash(ls)".into()));
        c.pending_tools.push(("tool_a".into(), a_idx));
        let b_idx = c.transcript.len();
        c.push(Entry::Tool("● Grep(x)".into()));
        c.pending_tools.push(("tool_b".into(), b_idx));

        c.insert_tool_result("tool_b", true, "grep out".into());
        c.insert_tool_result("tool_a", true, "ls out".into());

        let texts: Vec<String> = c
            .transcript
            .iter()
            .map(|e| match e {
                Entry::User(x) | Entry::Tool(x) => x.clone(),
                Entry::ToolResult { text, .. } => format!("⎿{text}"),
                _ => "?".into(),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["go", "● Bash(ls)", "⎿ls out", "● Grep(x)", "⎿grep out"]
        );
        assert!(c.pending_tools.is_empty());
    }

    #[test]
    fn unknown_tool_result_appends() {
        let mut c = Chat::blank("sid".into(), "t".into(), true, 0.0);
        c.push(Entry::User("go".into()));
        c.insert_tool_result("mystery", false, "boom".into());
        assert!(matches!(
            c.transcript.last(),
            Some(Entry::ToolResult { ok: false, .. })
        ));
    }

    #[test]
    fn cap_tool_text_keeps_head_and_tail() {
        let big: String = (0..1000).map(|i| format!("line{i}\n")).collect();
        let capped = cap_tool_text(&big);
        assert!(capped.contains("line0"));
        assert!(capped.contains("line999"));
        assert!(capped.contains("lines omitted"));
        assert!(capped.len() < big.len());
        // small results pass through untouched
        assert_eq!(cap_tool_text("ok\ndone"), "ok\ndone");
    }

    #[test]
    fn grapheme_helpers_never_split_emoji() {
        let s = "a👩‍👩‍👧‍👦b"; // family emoji is one grapheme, many bytes
        let after_a = 1;
        let after_emoji = next_grapheme(s, after_a);
        assert_eq!(&s[after_emoji..], "b");
        assert_eq!(prev_grapheme(s, after_emoji), after_a);
        assert_eq!(prev_grapheme(s, 0), 0);
        assert_eq!(next_grapheme(s, s.len()), s.len());
    }

    #[test]
    fn clean_line_strips_bidi_and_escapes() {
        assert_eq!(clean_line("ok\u{202E}evil"), "okevil");
        assert_eq!(clean_line("a\x1b[31mred\x1b[0m"), "ared");
        // a stray ESC before unicode text must not eat the next char
        assert_eq!(clean_line("\u{1b}日本"), "日本");
    }

    #[test]
    fn build_diff_edit_and_write() {
        let edit = serde_json::json!({
            "file_path": "/tmp/x.rs", "old_string": "foo", "new_string": "bar"
        });
        let (file, a, r, _) = build_diff("Edit", &edit).unwrap();
        assert_eq!((a, r), (1, 1));
        assert!(file.ends_with("x.rs"));

        let write = serde_json::json!({ "file_path": "/tmp/y.rs", "content": "l1\nl2\nl3" });
        let (_, a, r, _) = build_diff("Write", &write).unwrap();
        assert_eq!((a, r), (3, 0));

        assert!(build_diff("Bash", &serde_json::json!({"command":"ls"})).is_none());
    }
}
