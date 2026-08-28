//! App state + update logic — spaces, codex-simple chrome, vim modal.
//!
//! A SPACE is the sidebar unit: a named container of 1–2 chats (two render as
//! a thin-divider vsplit; `:vs` splits, Ctrl-w moves, `:q` closes). Each chat
//! is its own long-lived claude child. Modal: Insert composes (the launch
//! state), Esc drops to Normal, `:` runs ex commands, `ga` fuzzy-jumps spaces,
//! `:tasks` opens the board, `:diff` opens the diff pad (Ctrl-j/k walks
//! history). Chrome stays minimal, codex-style — cost/model/permissions live
//! behind `:cost` / `:status`, not on screen. Persistence per tmux session.

use std::path::{Path, PathBuf};
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
    /// Navigate: scroll, jump sessions, drive the sidebar. No text reaches the
    /// composer or the agent.
    Normal,
    /// Compose (the launch state): the composer is live; Enter sends.
    Insert,
    Rename,
    /// `:` ex commands (:q, :new, :tasks, …).
    Command,
    Confirm,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus {
    Sidebar,
    Main,
}

/// Full-screen overlays, drawn over the main surface and closed with Esc.
#[derive(PartialEq, Clone, Copy)]
pub enum Overlay {
    None,
    Help,
    /// `ga` — fuzzy space picker.
    Picker,
    /// `gd` / bare `:cd` — fuzzy directory picker rooted at $HOME.
    DirPicker,
    /// `:tasks` / Space t — one row per chat's current work.
    Board,
    /// `:diff` — quick pad over the focused chat's edit history.
    DiffPad,
}

/// Multi-key sequences in Normal mode (g-prefix, Space leader, Ctrl-w).
#[derive(PartialEq, Clone, Copy)]
enum Pending {
    None,
    G,
    Leader,
    LeaderE,
    CtrlW,
}

#[derive(PartialEq, Clone, Copy)]
enum ConfirmWhat {
    Quit,
    DeleteSpace(usize),
}

/// Sidebar/board status, derived from live state — never stored.
#[derive(PartialEq, Clone, Copy)]
pub enum ChatStatus {
    Running,
    Error,
    Idle,
    New,
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

/// How many files the diff pad holds, newest last. Each is already capped to
/// ~151 lines by `cap_diff`, so this bounds the pad at roughly 6k rows.
const PAD_MAX_FILES: usize = 40;

/// One row of the diff pad's continuous document. `line: None` marks the
/// separator that introduces a file.
pub struct DocRow {
    pub file: String,
    pub added: usize,
    pub removed: usize,
    pub line: Option<DiffLine>,
    /// New-side line number, blank on deletions and elisions — codediff renders
    /// removed content as unnumbered virtual rows. Hunk-relative: an Edit tool
    /// call carries only old_string/new_string, so absolute file positions
    /// aren't recoverable. Accurate for Write (whole file), a ruler otherwise.
    pub num: Option<u32>,
    /// Byte range within `line.text` that actually differs from its paired
    /// line — codediff's tier-2 "char" highlight, punched through the wash.
    pub hl: Option<(usize, usize)>,
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

/// The sidebar unit: a named container of one or two chats. Two chats render
/// as a thin-divider vsplit; `focused` is the pane the composer talks to.
pub struct Space {
    pub name: String,
    pub chats: Vec<Chat>,
    pub focused: usize,
    /// Working directory for every chat in this space — the cwd each claude
    /// child is spawned in (Saxon, 2026-08-27: "in a space i should be able
    /// to open into a dir"). Set with `:cd`, or `gd` for the fuzzy picker.
    /// Per-space, so two spaces can sit in two different repos at once.
    pub dir: PathBuf,
}

impl Space {
    fn of(chat: Chat) -> Self {
        Space {
            name: String::new(),
            chats: vec![chat],
            focused: 0,
            dir: default_dir(),
        }
    }

    pub fn focused_chat(&self) -> &Chat {
        &self.chats[self.focused.min(self.chats.len() - 1)]
    }

    /// Aggregate status for the sidebar glyph: running beats error beats idle.
    pub fn status(&self) -> ChatStatus {
        if self.chats.iter().any(|c| c.in_flight) {
            ChatStatus::Running
        } else if self.chats.iter().any(|c| c.status() == ChatStatus::Error) {
            ChatStatus::Error
        } else if self.chats.iter().all(|c| c.transcript.is_empty()) {
            ChatStatus::New
        } else {
            ChatStatus::Idle
        }
    }
}

/// Display name for a space ("new space" until named).
pub fn space_name(sp: &Space) -> String {
    if sp.name.trim().is_empty() {
        "new space".to_string()
    } else {
        sp.name.clone()
    }
}

pub struct Chat {
    /// Stable id on the Msg wire — agent events route back to THIS chat.
    pub id: u64,
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
    /// Ctrl-t — render tool results in full instead of one summary line.
    pub expand_tools: bool,
    /// In-flight tool calls: (tool_use_id, transcript index of the ● Tool entry),
    /// so each result lands under ITS call even when calls run in parallel.
    pub pending_tools: Vec<(String, usize)>,
    /// Pre-wrapped visual rows of the settled transcript, keyed by (rev, width,
    /// expand_tools). Streaming only rebuilds the tail — never this.
    pub cache: crate::ui::RenderCache,
}

impl Chat {
    fn fresh(id: u64) -> Self {
        let session_id = Uuid::new_v4().to_string();
        Chat::blank(id, session_id, true, 0.0)
    }

    fn from_persist(id: u64, pc: &PersistChat) -> Self {
        // `started` decides --session-id vs --resume on the next spawn: a chat
        // that never actually sent a turn must NOT try to resume a session that
        // claude has no record of.
        let mut c = Chat::blank(id, pc.session_id.clone(), !pc.started, pc.cost);
        if !pc.transcript.is_empty() {
            c.transcript = pc.transcript.clone();
            c.rev += 1;
        }
        c
    }

    fn blank(id: u64, session_id: String, first_turn: bool, cost: f64) -> Self {
        Chat {
            id,
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

    /// Live status for the sidebar glyph and the board row.
    pub fn status(&self) -> ChatStatus {
        if self.in_flight {
            ChatStatus::Running
        } else if matches!(self.transcript.last(), Some(Entry::Error(_))) {
            ChatStatus::Error
        } else if self.transcript.is_empty() {
            ChatStatus::New
        } else {
            ChatStatus::Idle
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

/// True when `name` is still an auto-generated default or empty — used so the
/// first rename starts from an empty buffer, and auto-naming can take over.
fn is_default_name(name: &str) -> bool {
    let name = name.trim();
    name.is_empty() || name == "new space"
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

/// Where a new space starts: the directory aeovim itself was launched in.
fn default_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| home_dir())
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// `~/src/foo` rather than `/Users/saxon/src/foo` — the form worth showing.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(home_dir()) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Expand a leading `~`, and resolve relative paths against the space's
/// current dir so `:cd ../sibling` behaves the way a shell would. Returns
/// None unless the result is a real directory — `:cd` never invents a path.
fn resolve_dir(input: &str, base: &Path) -> Option<PathBuf> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let p = if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        let p = PathBuf::from(s);
        if p.is_absolute() { p } else { base.join(p) }
    };
    std::fs::canonicalize(&p).ok().filter(|p| p.is_dir())
}

/// Directories under $HOME, for the `gd` picker. Bounded on purpose: walking
/// a whole home directory is unbounded work and mostly noise. Depth 3 with
/// the heavy and hidden trees skipped covers where projects actually live,
/// and keeps the scan fast enough to run on open.
fn scan_dirs() -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        "Library",
        "Applications",
        "Pictures",
        "Movies",
        "Music",
        "Public",
    ];
    const CAP: usize = 4000;

    let home = home_dir();
    let mut out = vec![home.clone()];
    let mut queue = vec![(home, 0usize)];
    while let Some((dir, depth)) = queue.pop() {
        if depth >= 3 || out.len() >= CAP {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let p = e.path();
            out.push(p.clone());
            queue.push((p, depth + 1));
        }
    }
    out.sort();
    out.dedup();
    out
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

/// The byte ranges of the differing middle of two paired lines: trim the common
/// prefix and the common suffix, and what's left is what changed. Returns
/// (range in `a`, range in `b`), or None if they're identical.
///
/// codediff computes a real character-level diff; prefix/suffix trimming lands
/// on the same span for the edits people actually make (a renamed identifier, a
/// changed argument) and costs O(n) instead of O(n·m) per line pair.
fn inner_change(a: &str, b: &str) -> Option<((usize, usize), (usize, usize))> {
    if a == b {
        return None;
    }
    // Walk chars, not bytes — a range that splits a multi-byte char would
    // panic the moment the renderer sliced on it.
    let mut p = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        p += ca.len_utf8();
    }
    let mut s = 0;
    let (mut ra, mut rb) = (a[p..].chars().rev(), b[p..].chars().rev());
    loop {
        match (ra.next(), rb.next()) {
            (Some(x), Some(y)) if x == y => s += x.len_utf8(),
            _ => break,
        }
    }
    Some(((p, a.len() - s), (p, b.len() - s)))
}

/// Line numbers + intra-line change ranges for one file's hunk.
///
/// Numbering counts the new side (context and additions), so deletions and
/// elisions come back as None. Intra-line ranges pair the k-th deletion of a
/// run with the k-th addition that follows it — the same "this line became
/// that line" assumption codediff's mapping makes.
fn annotate(lines: &[DiffLine]) -> Vec<(Option<u32>, Option<(usize, usize)>)> {
    let mut out: Vec<(Option<u32>, Option<(usize, usize)>)> = Vec::with_capacity(lines.len());
    let mut n: u32 = 0;
    for l in lines {
        match l.kind {
            DiffKind::Ctx | DiffKind::Add => {
                n += 1;
                out.push((Some(n), None));
            }
            DiffKind::Del | DiffKind::Gap => out.push((None, None)),
        }
    }
    // Pair each Del run with the Add run directly following it.
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != DiffKind::Del {
            i += 1;
            continue;
        }
        let dstart = i;
        while i < lines.len() && lines[i].kind == DiffKind::Del {
            i += 1;
        }
        let astart = i;
        while i < lines.len() && lines[i].kind == DiffKind::Add {
            i += 1;
        }
        let pairs = (astart - dstart).min(i - astart);
        for k in 0..pairs {
            let (d, a) = (dstart + k, astart + k);
            if let Some((rd, ra)) = inner_change(&lines[d].text, &lines[a].text) {
                out[d].1 = Some(rd);
                out[a].1 = Some(ra);
            }
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
    /// An event from one session's claude child, routed by chat id.
    Agent { chat: u64, ev: AgentEvent },
    /// The claude child exited (crash, error, or deliberate kill).
    SessionEnded { chat: u64, error: Option<String> },
    /// Clipboard contents read off the UI task (Ctrl-v fallback).
    Pasted(String),
}

pub struct App {
    pub mode: Mode,
    pub input: String,
    /// Cursor position in `input`: a byte offset on a grapheme boundary.
    pub input_cursor: usize,
    pub rename_buf: String,
    /// Which space `rename_buf` is renaming (a sidebar rename can target a
    /// non-active row).
    pub rename_target: usize,
    pub spaces: Vec<Space>,
    pub active: usize,
    next_id: u64,
    pub focus: Focus,
    pub sidebar_open: bool,
    pub sidebar_cursor: usize,
    pub overlay: Overlay,
    pending: Pending,
    pub cmdline: String,
    pub picker_query: String,
    pub picker_sel: usize,
    pub dir_query: String,
    pub dir_sel: usize,
    /// Previous dir of the active space, for `:cd -`.
    prev_dir: Option<PathBuf>,
    /// Scanned once per picker open, not per keystroke.
    pub dir_candidates: Vec<PathBuf>,
    pub board_sel: usize,
    /// Scroll within the selected file's body (not the whole document — the
    /// pad shows one file at a time, the way codediff's explorer drives it).
    pub diff_scroll: usize,
    /// Which file the explorer panel has selected.
    pub diff_sel: usize,
    /// Is the file panel showing? `Tab` toggles it (codediff's `<leader>b`).
    pub diff_panel: bool,
    /// Half-typed `]`/`[` waiting for its `c` or `f`.
    diff_pending: Option<char>,
    /// Cached pad document; see `sync_diff_doc`. Cleared when the pad closes.
    pub diff_rows: Vec<DocRow>,
    pub diff_starts: Vec<usize>,
    pub diff_elided: usize,
    diff_key: Option<(u64, u64)>,
    confirm_what: ConfirmWhat,
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
    /// Mouse capture state. Default OFF so the cursor can select/copy text (and
    /// tmux/Ghostty native selection works); `/mouse` toggles wheel-scroll on.
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
        // Adopt every saved space. Legacy shapes migrate: an unnamed space with
        // many chats (the flat multi-session save) splits into one space per
        // chat, named by the chat's old title; an oversize space (>2 chats)
        // spills its extras into their own spaces. Nothing is dropped.
        let mut next_id: u64 = 1;
        let mut spaces: Vec<Space> = Vec::new();
        for sp in &restored {
            let named = !sp.name.trim().is_empty();
            if named && sp.chats.len() <= 2 && !sp.chats.is_empty() {
                let mut chats = Vec::new();
                for pc in &sp.chats {
                    chats.push(Chat::from_persist(next_id, pc));
                    next_id += 1;
                }
                spaces.push(Space {
                    name: sp.name.clone(),
                    chats,
                    focused: 0,
                    // A saved dir that has since been moved or deleted falls
                    // back rather than spawning sessions into nowhere.
                    dir: sp
                        .dir
                        .as_deref()
                        .map(PathBuf::from)
                        .filter(|p| p.is_dir())
                        .unwrap_or_else(default_dir),
                });
            } else {
                for pc in &sp.chats {
                    let c = Chat::from_persist(next_id, pc);
                    next_id += 1;
                    let mut s = Space::of(c);
                    s.name = pc.title.clone();
                    spaces.push(s);
                }
            }
        }
        if spaces.is_empty() {
            spaces.push(Space::of(Chat::fresh(next_id)));
            next_id += 1;
        }

        // The launch directory decides which space you land in (Saxon,
        // 2026-08-27): if a saved space is already pointed at this directory,
        // reopen THAT one; otherwise this directory gets a space of its own.
        // So `cd ~/foo && avim` resumes the space you last worked in there —
        // including one you created in ~ and later `:cd`'d into ~/foo —
        // rather than whatever happened to be active when you quit.
        let here = default_dir();
        let active = match spaces.iter().position(|s| s.dir == here) {
            Some(i) => i,
            None => {
                spaces.push(Space::of(Chat::fresh(next_id)));
                next_id += 1;
                spaces.len() - 1
            }
        };

        Self {
            mode: Mode::Insert, // launch ready to type
            input: String::new(),
            input_cursor: 0,
            rename_buf: String::new(),
            rename_target: 0,
            spaces,
            active,
            next_id,
            focus: Focus::Main,
            sidebar_open: true,
            sidebar_cursor: active,
            overlay: Overlay::None,
            pending: Pending::None,
            cmdline: String::new(),
            picker_query: String::new(),
            picker_sel: 0,
            dir_query: String::new(),
            dir_sel: 0,
            prev_dir: None,
            dir_candidates: Vec::new(),
            board_sel: 0,
            diff_scroll: 0,
            diff_sel: 0,
            diff_panel: true,
            diff_pending: None,
            diff_rows: Vec::new(),
            diff_starts: Vec::new(),
            diff_elided: 0,
            diff_key: None,
            confirm_what: ConfirmWhat::Quit,
            confirm_msg: String::new(),
            dirty: true,
            toast: None,
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            mouse_capture: false,
            slash_commands: Vec::new(),
            slash_sel: 0,
            workspace_key,
            tx,
        }
    }

    /// The active space.
    pub fn space(&self) -> &Space {
        &self.spaces[self.active]
    }

    fn space_mut(&mut self) -> &mut Space {
        let i = self.active;
        &mut self.spaces[i]
    }

    /// The focused chat of the active space — what the composer talks to.
    pub fn chat(&self) -> &Chat {
        self.spaces[self.active].focused_chat()
    }

    fn chat_mut(&mut self) -> &mut Chat {
        let i = self.active;
        let sp = &mut self.spaces[i];
        let f = sp.focused.min(sp.chats.len() - 1);
        &mut sp.chats[f]
    }

    /// Locate a chat by wire id → (space index, chat index).
    fn find_chat(&self, id: u64) -> Option<(usize, usize)> {
        for (si, sp) in self.spaces.iter().enumerate() {
            for (ci, c) in sp.chats.iter().enumerate() {
                if c.id == id {
                    return Some((si, ci));
                }
            }
        }
        None
    }

    /// Every chat as a flat (space, chat) list — the board's row order.
    pub fn board_rows(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (si, sp) in self.spaces.iter().enumerate() {
            for ci in 0..sp.chats.len() {
                out.push((si, ci));
            }
        }
        out
    }

    pub fn any_in_flight(&self) -> bool {
        self.spaces.iter().any(|s| s.chats.iter().any(|c| c.in_flight))
    }

    /// Spaces with work running (the sidebar rollup).
    pub fn running_count(&self) -> usize {
        self.spaces
            .iter()
            .filter(|s| s.chats.iter().any(|c| c.in_flight))
            .count()
    }

    /// Total cost across every chat (shown only via :cost — codex-simple).
    pub fn total_cost(&self) -> f64 {
        self.spaces
            .iter()
            .map(|s| s.chats.iter().map(|c| c.cost).sum::<f64>())
            .sum()
    }

    /// The focused chat's diffs, newest first: (file, +lines, −lines,
    /// transcript index of the Diff entry).
    /// Every diff in this chat flattened into ONE scrollable document,
    /// oldest first, each file introduced by a separator row (Saxon,
    /// 2026-08-27: "any file that is changed in the session should be able to
    /// see the diff, and while i scroll if i go to a different file diff it
    /// will go to said file and rename the header").
    ///
    /// Each row carries its own file, so the header is just a lookup of
    /// whatever row sits at the top of the viewport — the name can never
    /// drift out of sync with what's on screen.
    /// Rebuild the pad's document only when the underlying chat actually
    /// changed. The document owns copies of its rows, so building it per
    /// frame (which the first cut did — twice, since the starts list rebuilt
    /// it too) meant cloning every diff line ~60×/second for as long as the
    /// pad was open. Keyed on (chat id, rev) so switching chats or a new edit
    /// arriving refreshes it, and nothing else does.
    pub fn sync_diff_doc(&mut self) {
        let key = (self.chat().id, self.chat().rev);
        if self.diff_key == Some(key) {
            return;
        }

        // Only the most recent PAD_MAX_FILES files are held. A long session
        // can touch hundreds of files, and the pad is for reviewing what just
        // happened — the cap bounds both the build cost and what the pad
        // retains, without touching what's stored in the transcript.
        let all: Vec<&Entry> = self
            .chat()
            .transcript
            .iter()
            .filter(|e| matches!(e, Entry::Diff { .. }))
            .collect();
        let elided = all.len().saturating_sub(PAD_MAX_FILES);

        let mut rows = Vec::new();
        let mut starts = Vec::new();
        for e in all.into_iter().skip(elided) {
            let Entry::Diff { file, added, removed, lines } = e else { continue };
            starts.push(rows.len());
            rows.push(DocRow {
                file: file.clone(),
                added: *added,
                removed: *removed,
                line: None,
                num: None,
                hl: None,
            });
            for (l, (num, hl)) in lines.iter().zip(annotate(lines)) {
                rows.push(DocRow {
                    file: file.clone(),
                    added: *added,
                    removed: *removed,
                    line: Some(l.clone()),
                    num,
                    hl,
                });
            }
        }
        self.diff_rows = rows;
        self.diff_starts = starts;
        self.diff_elided = elided;
        self.diff_key = Some(key);
        // Default to the newest file, the way the pad always opened newest-first.
        if self.diff_sel >= self.diff_starts.len() {
            self.diff_sel = self.diff_starts.len().saturating_sub(1);
        }
    }

    /// Row range `[start, end)` of the selected file's body, skipping the
    /// separator row that introduces it.
    pub fn diff_file_rows(&self) -> (usize, usize) {
        let Some(&start) = self.diff_starts.get(self.diff_sel) else {
            return (0, 0);
        };
        let end = self
            .diff_starts
            .get(self.diff_sel + 1)
            .copied()
            .unwrap_or(self.diff_rows.len());
        (start + 1, end)
    }

    /// Select file `idx` and land on its first change — codediff's
    /// `jump_to_first_change`, which the user has enabled.
    fn diff_goto_file(&mut self, idx: usize) {
        if self.diff_starts.is_empty() {
            return;
        }
        self.diff_sel = idx.min(self.diff_starts.len() - 1);
        let (start, end) = self.diff_file_rows();
        let first = (start..end)
            .position(|r| {
                self.diff_rows[r]
                    .line
                    .as_ref()
                    .is_some_and(|l| matches!(l.kind, DiffKind::Add | DiffKind::Del))
            })
            .unwrap_or(0);
        self.diff_scroll = first.saturating_sub(3);
    }

    /// Next (`dir > 0`) or previous change block within the selected file —
    /// codediff's `]c` / `[c`. Blocks are runs of Add/Del, so a delete
    /// immediately followed by its replacement counts once.
    fn diff_hunk(&mut self, dir: isize) {
        let (start, end) = self.diff_file_rows();
        if start >= end {
            return;
        }
        let changed = |r: usize| -> bool {
            self.diff_rows[r]
                .line
                .as_ref()
                .is_some_and(|l| matches!(l.kind, DiffKind::Add | DiffKind::Del))
        };
        // Block starts: a changed row whose predecessor wasn't changed.
        let heads: Vec<usize> = (start..end)
            .filter(|&r| changed(r) && (r == start || !changed(r - 1)))
            .collect();
        if heads.is_empty() {
            return;
        }
        // diff_scroll is relative to the file body; heads are absolute rows.
        let cur = start + self.diff_scroll;
        let target = if dir > 0 {
            heads.iter().find(|&&h| h > cur + 3).copied()
        } else {
            heads.iter().rev().find(|&&h| h + 3 < cur).copied()
        };
        if let Some(t) = target {
            self.diff_scroll = (t - start).saturating_sub(3);
        }
    }

    /// Drop the cached document when the pad closes — it can be a few hundred
    /// KB and nothing reads it while the overlay is down.
    fn clear_diff_doc(&mut self) {
        self.diff_rows = Vec::new();
        self.diff_starts = Vec::new();
        self.diff_key = None;
        self.diff_elided = 0;
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
        let data: Vec<PersistSpace> = self
            .spaces
            .iter()
            .map(|sp| PersistSpace {
                name: space_name(sp),
                dir: Some(sp.dir.display().to_string()),
                chats: sp
                    .chats
                    .iter()
                    .map(|c| PersistChat {
                        title: String::new(),
                        session_id: c.session_id.clone(),
                        cost: c.cost,
                        started: !c.first_turn,
                        transcript: c.transcript.clone(),
                    })
                    .collect(),
            })
            .collect();
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
            Msg::Agent { chat, ev } => self.handle_agent(chat, ev),
            Msg::SessionEnded { chat, error } => self.session_ended(chat, error),
        }
        self.dirty = true;
    }

    /// The claude child exited. A clean exit (deliberate kill, stdin close) is
    /// quiet; a crash mid-turn surfaces the error. A stale `--resume`
    /// self-heals once by re-minting the session and replaying the last prompt.
    fn session_ended(&mut self, chat: u64, error: Option<String>) {
        let Some((si, ci)) = self.find_chat(chat) else {
            return; // chat was deleted while its child wound down
        };
        if error
            .as_deref()
            .is_some_and(|e| e.contains("No conversation found with session ID"))
        {
            let replay = {
                let c = &mut self.spaces[si].chats[ci];
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
                self.deliver(si, ci, p, false);
                return;
            }
        }
        let c = &mut self.spaces[si].chats[ci];
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
        self.drain_queue(si, ci);
    }

    fn handle_agent(&mut self, chat: u64, ev: AgentEvent) {
        let Some((si, ci)) = self.find_chat(chat) else {
            return; // late event for a deleted chat
        };
        let is_result = matches!(ev, AgentEvent::TurnResult { .. });
        let c = &mut self.spaces[si].chats[ci];
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
            self.drain_queue(si, ci);
        }
    }

    // ---- input ----

    fn handle_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.toast = None; // any keypress clears the statusline notice
        match self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                let close_key = matches!(
                    k.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ')
                );
                if close_key {
                    return;
                }
                // other keys close the overlay AND still do their thing
            }
            Overlay::Picker => {
                self.key_picker(k);
                return;
            }
            Overlay::DirPicker => {
                self.key_dir_picker(k);
                return;
            }
            Overlay::Board => {
                self.key_board(k);
                return;
            }
            Overlay::DiffPad => {
                self.key_diffpad(k);
                return;
            }
            Overlay::None => {}
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Normal => self.key_normal(k, ctrl),
            Mode::Rename => self.key_rename(k, ctrl),
            Mode::Insert => self.key_insert(k, ctrl),
            Mode::Command => self.key_command(k, ctrl),
            Mode::Confirm => self.key_confirm(k),
        }
    }

    // ---- Normal mode: navigate, jump sessions, drive the sidebar ----

    fn key_normal(&mut self, k: KeyEvent, ctrl: bool) {
        // multi-key sequences (gg/gt/gT/ga, Space leader) resolve first
        let pend = self.pending;
        self.pending = Pending::None;
        match pend {
            Pending::G => {
                match k.code {
                    KeyCode::Char('g') => self.scroll_top(),
                    KeyCode::Char('t') => self.next_space(1),
                    KeyCode::Char('T') => self.next_space(-1),
                    KeyCode::Char('a') => self.open_picker(),
                    KeyCode::Char('d') => self.open_dir_picker(),
                    _ => {}
                }
                return;
            }
            Pending::Leader => {
                match k.code {
                    KeyCode::Char(c @ '1'..='9') => self.jump_space(c as usize - '1' as usize),
                    KeyCode::Char('0') => self.jump_space(9),
                    KeyCode::Char('e') => self.pending = Pending::LeaderE,
                    KeyCode::Char('n') => self.new_space(),
                    KeyCode::Char('t') => self.open_board(),
                    KeyCode::Enter => self.send_prompt(),
                    _ => {}
                }
                return;
            }
            Pending::LeaderE => {
                match k.code {
                    KeyCode::Char('e') => self.toggle_sidebar(),
                    KeyCode::Char('f') => self.focus_sidebar(),
                    _ => {}
                }
                return;
            }
            Pending::CtrlW => {
                // vim window commands over the space's panes
                match k.code {
                    KeyCode::Char('v') | KeyCode::Char('s') => self.add_split(),
                    KeyCode::Char('h') => self.focus_pane(0),
                    KeyCode::Char('l') => self.focus_pane(1),
                    KeyCode::Char('w') => self.cycle_pane(),
                    KeyCode::Char('q') | KeyCode::Char('c') => self.close_focused_pane(),
                    _ => {}
                }
                return;
            }
            Pending::None => {}
        }
        if ctrl {
            match k.code {
                KeyCode::Char('w') => self.pending = Pending::CtrlW,
                // Ctrl-h/l walk sidebar ← pane ← → pane, like the old layout.
                KeyCode::Char('h') => {
                    let sp = self.space();
                    if self.focus == Focus::Main && sp.chats.len() > 1 && sp.focused > 0 {
                        self.focus_pane(0);
                    } else {
                        self.focus_sidebar();
                    }
                }
                KeyCode::Char('l') => {
                    if self.focus == Focus::Sidebar {
                        self.focus = Focus::Main;
                    } else {
                        self.focus_pane(1);
                    }
                }
                KeyCode::Char('j') if self.focus == Focus::Sidebar => self.sidebar_move(1),
                KeyCode::Char('k') if self.focus == Focus::Sidebar => self.sidebar_move(-1),
                KeyCode::Char('d') => self.scroll_down(12),
                KeyCode::Char('u') => self.scroll_up(12),
                KeyCode::Char('t') => {
                    let c = self.chat_mut();
                    c.expand_tools = !c.expand_tools;
                }
                KeyCode::Char('c') => {
                    if self.chat().in_flight {
                        self.interrupt();
                    } else {
                        self.confirm_quit();
                    }
                }
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc => {
                if self.chat().in_flight {
                    self.interrupt();
                } else {
                    self.chat_mut().follow = true;
                }
            }
            KeyCode::Char('i') => self.enter_insert(false),
            KeyCode::Char('a') => self.enter_insert(true),
            KeyCode::Char('o') => {
                self.enter_insert(true);
                if !self.input.is_empty() {
                    self.insert_at_cursor("\n");
                }
            }
            KeyCode::Char(':') => {
                self.cmdline.clear();
                self.mode = Mode::Command;
            }
            KeyCode::Char('/') => {
                // Always type the slash — on an empty draft it opens the
                // command popup, otherwise it's just a character. Dropping it
                // when a draft existed made the keystroke vanish silently.
                self.enter_insert(true);
                self.insert_at_cursor("/");
            }
            KeyCode::Char(' ') => self.pending = Pending::Leader,
            KeyCode::Char('g') => self.pending = Pending::G,
            KeyCode::Char('G') | KeyCode::End => self.chat_mut().follow = true,
            KeyCode::Home => self.scroll_top(),
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Sidebar => self.sidebar_move(1),
                Focus::Main => self.scroll_down(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Sidebar => self.sidebar_move(-1),
                Focus::Main => self.scroll_up(1),
            },
            KeyCode::PageDown => self.scroll_down(12),
            KeyCode::PageUp => self.scroll_up(12),
            KeyCode::Enter if self.focus == Focus::Sidebar => {
                self.jump_space(self.sidebar_cursor)
            }
            KeyCode::Char('n') if self.focus == Focus::Sidebar => self.new_space(),
            KeyCode::Char('d') if self.focus == Focus::Sidebar => {
                self.confirm_delete_space(self.sidebar_cursor)
            }
            KeyCode::Char('r') => {
                let target = if self.focus == Focus::Sidebar {
                    self.sidebar_cursor
                } else {
                    self.active
                };
                self.start_rename(target);
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
            _ => {}
        }
    }

    fn enter_insert(&mut self, at_end: bool) {
        self.focus = Focus::Main;
        self.mode = Mode::Insert;
        if at_end {
            self.input_cursor = self.input.len();
        }
    }

    fn scroll_top(&mut self) {
        let c = self.chat_mut();
        c.follow = false;
        c.scroll = 0;
    }

    // ---- sidebar, spaces & panes ----

    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.focus_sidebar();
        } else {
            self.focus = Focus::Main;
        }
    }

    fn focus_sidebar(&mut self) {
        self.sidebar_open = true;
        self.sidebar_cursor = self.active.min(self.spaces.len().saturating_sub(1));
        self.focus = Focus::Sidebar;
        self.mode = Mode::Normal;
    }

    fn sidebar_move(&mut self, d: isize) {
        let n = self.spaces.len();
        if n == 0 {
            return;
        }
        let cur = self.sidebar_cursor.min(n - 1) as isize;
        self.sidebar_cursor = (cur + d).rem_euclid(n as isize) as usize;
    }

    fn jump_space(&mut self, idx: usize) {
        if idx < self.spaces.len() {
            self.active = idx;
            self.sidebar_cursor = idx;
            self.focus = Focus::Main;
        } else {
            self.toast = Some(format!("no space {}", idx + 1));
        }
    }

    fn next_space(&mut self, d: isize) {
        let n = self.spaces.len();
        if n == 0 {
            return;
        }
        let next = (self.active as isize + d).rem_euclid(n as isize) as usize;
        self.jump_space(next);
    }

    fn new_space(&mut self) {
        let c = Chat::fresh(self.next_id);
        self.next_id += 1;
        self.spaces.push(Space::of(c));
        self.jump_space(self.spaces.len() - 1);
        self.mode = Mode::Insert; // a fresh space: start typing
        self.persist();
    }

    /// `:vs` / Ctrl-w v — add a second chat pane to the active space.
    fn add_split(&mut self) {
        if self.space().chats.len() >= 2 {
            self.toast = Some("space already has two chats".into());
            return;
        }
        let c = Chat::fresh(self.next_id);
        self.next_id += 1;
        let sp = self.space_mut();
        sp.chats.push(c);
        sp.focused = sp.chats.len() - 1;
        self.focus = Focus::Main;
        self.mode = Mode::Insert; // new pane: start typing
        self.persist();
    }

    fn focus_pane(&mut self, pane: usize) {
        let sp = self.space_mut();
        if pane < sp.chats.len() {
            sp.focused = pane;
        }
        self.focus = Focus::Main;
    }

    fn cycle_pane(&mut self) {
        let sp = self.space_mut();
        if sp.chats.len() > 1 {
            sp.focused = (sp.focused + 1) % sp.chats.len();
        }
        self.focus = Focus::Main;
    }

    /// `:q` / Ctrl-w q — close the focused pane; the space when that was its
    /// last pane; the app when that was the last space. Straight vim window
    /// semantics, and deliberately NOT confirmed (Saxon, 2026-08-27: ":q
    /// should just quit, not ask to confirm quitting").
    ///
    /// The accident-prone paths still ask: Ctrl-c on an idle chat and Ctrl-D
    /// on an empty composer both route through `confirm_quit`. Typing `:q` is
    /// deliberate in a way that a stray control key is not.
    fn close_focused_pane(&mut self) {
        if self.space().chats.len() > 1 {
            let ci = self.space().focused;
            self.close_pane(self.active, ci);
        } else if self.spaces.len() > 1 {
            self.delete_space(self.active);
        } else {
            // Last pane of the last space: nothing left to return to.
            self.should_quit = true;
        }
    }

    fn close_pane(&mut self, si: usize, ci: usize) {
        let Some(sp) = self.spaces.get_mut(si) else { return };
        if ci >= sp.chats.len() || sp.chats.len() == 1 {
            return;
        }
        let mut c = sp.chats.remove(ci);
        if let Some(s) = c.session.take() {
            s.kill();
        }
        sp.focused = sp.focused.min(sp.chats.len() - 1);
        self.persist();
    }

    fn confirm_delete_space(&mut self, idx: usize) {
        if idx >= self.spaces.len() {
            return;
        }
        let name = space_name(&self.spaces[idx]);
        let running = if self.spaces[idx].chats.iter().any(|c| c.in_flight) {
            " — a turn is running"
        } else {
            ""
        };
        self.confirm_msg = format!("delete space \"{name}\"{running}?   y / n");
        self.confirm_what = ConfirmWhat::DeleteSpace(idx);
        self.mode = Mode::Confirm;
    }

    fn delete_space(&mut self, idx: usize) {
        if idx >= self.spaces.len() {
            return;
        }
        let mut sp = self.spaces.remove(idx);
        for c in &mut sp.chats {
            if let Some(s) = c.session.take() {
                s.kill();
            }
        }
        if self.spaces.is_empty() {
            self.spaces.push(Space::of(Chat::fresh(self.next_id)));
            self.next_id += 1;
        }
        if self.active > idx {
            self.active -= 1;
        }
        self.active = self.active.min(self.spaces.len() - 1);
        self.sidebar_cursor = self.sidebar_cursor.min(self.spaces.len() - 1);
        self.persist();
    }

    fn start_rename(&mut self, idx: usize) {
        if idx >= self.spaces.len() {
            return;
        }
        self.rename_target = idx;
        let t = &self.spaces[idx].name;
        self.rename_buf = if is_default_name(t) { String::new() } else { t.clone() };
        self.mode = Mode::Rename;
    }

    fn key_rename(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let name = self.rename_buf.trim().to_string();
                if !name.is_empty() {
                    if let Some(sp) = self.spaces.get_mut(self.rename_target) {
                        sp.name = name;
                    }
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
            KeyCode::Char(c) if !ctrl => self.rename_buf.push(c),
            _ => {}
        }
    }

    // ---- Command mode (`:`) ----

    fn key_command(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Char('u') if ctrl => self.cmdline.clear(),
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Enter => {
                let cmd = self.cmdline.trim().to_string();
                self.cmdline.clear();
                self.mode = Mode::Normal;
                self.exec_command(&cmd);
            }
            KeyCode::Backspace => {
                if self.cmdline.is_empty() {
                    self.mode = Mode::Normal;
                } else {
                    let at = self.cmdline.len();
                    let start = prev_grapheme(&self.cmdline, at);
                    self.cmdline.truncate(start);
                }
            }
            KeyCode::Char(c) if !ctrl => self.cmdline.push(c),
            _ => {}
        }
    }

    fn exec_command(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        // `:3` jumps to space 3, like `:b 3`.
        if let Ok(n) = cmd.parse::<usize>() {
            if n >= 1 {
                self.jump_space(n - 1);
            }
            return;
        }
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match head {
            // vim window semantics: :q closes the focused pane (then the
            // space, then the app); :qa quits outright. Neither prompts —
            // typing the command IS the confirmation.
            "q" => self.close_focused_pane(),
            "qa" => self.should_quit = true,
            "q!" | "qa!" | "quit" => self.should_quit = true,
            "vs" | "vsplit" | "sp" | "split" => self.add_split(),
            "new" | "n" => self.new_space(),
            "bd" => self.confirm_delete_space(self.active),
            // `:cd <path>` moves this space; bare `:cd` opens the picker.
            // `:cd -` is the shell idiom for "back where I was".
            "cd" => {
                if arg.is_empty() {
                    self.open_dir_picker();
                } else if arg == "-" {
                    match self.prev_dir.clone() {
                        Some(p) if p.is_dir() => self.set_space_dir(p),
                        _ => self.toast = Some("no previous directory".into()),
                    }
                } else {
                    let base = self.space().dir.clone();
                    match resolve_dir(arg, &base) {
                        Some(p) => self.set_space_dir(p),
                        None => self.toast = Some(format!("not a directory: {arg}")),
                    }
                }
            }
            "pwd" => self.toast = Some(tilde(&self.space().dir)),
            "clear" => self.reset_conversation(),
            "tasks" | "board" => self.open_board(),
            "diff" => self.open_diff_pad(),
            "cost" => {
                let sp: f64 = self.space().chats.iter().map(|c| c.cost).sum();
                self.toast = Some(format!(
                    "this space ${:.2} · all spaces ${:.2}",
                    sp,
                    self.total_cost()
                ));
            }
            "status" => {
                let perm = if self.dangerous { "dangerous" } else { "acceptEdits" };
                let sid = &self.chat().session_id[..8.min(self.chat().session_id.len())];
                self.toast = Some(format!(
                    "{} · {perm} · {} spaces · {sid} · ${:.2}",
                    self.model_display,
                    self.spaces.len(),
                    self.total_cost()
                ));
            }
            "rename" => {
                if arg.is_empty() {
                    self.start_rename(self.active);
                } else {
                    self.space_mut().name = arg.to_string();
                    self.persist();
                }
            }
            "help" => self.overlay = Overlay::Help,
            "mouse" => self.toggle_mouse(),
            "tools" => {
                let c = self.chat_mut();
                c.expand_tools = !c.expand_tools;
            }
            _ => self.toast = Some(format!(":{head}? unknown command")),
        }
    }

    // ---- :diff pad ----

    fn open_diff_pad(&mut self) {
        self.sync_diff_doc();
        if self.diff_starts.is_empty() {
            self.clear_diff_doc();
            self.toast = Some("no diffs in this chat yet".into());
            return;
        }
        // Open on the newest file, landing on its first change.
        self.diff_pending = None;
        self.diff_goto_file(self.diff_starts.len().saturating_sub(1));
        self.overlay = Overlay::DiffPad;
        self.mode = Mode::Normal;
    }

    fn key_diffpad(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let nfiles = self.diff_starts.len();
        if nfiles == 0 {
            self.overlay = Overlay::None;
            self.clear_diff_doc();
            return;
        }
        // `]`/`[` are two-key motions: ]c/[c change, ]f/[f file (codediff's).
        if let Some(br) = self.diff_pending.take() {
            let dir = if br == ']' { 1isize } else { -1 };
            match k.code {
                KeyCode::Char('c') => self.diff_hunk(dir),
                KeyCode::Char('f') => {
                    let next = if dir > 0 {
                        (self.diff_sel + 1).min(nfiles - 1)
                    } else {
                        self.diff_sel.saturating_sub(1)
                    };
                    self.diff_goto_file(next);
                }
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
                self.overlay = Overlay::None;
                self.clear_diff_doc();
            }
            KeyCode::Char(']') if !ctrl => self.diff_pending = Some(']'),
            KeyCode::Char('[') if !ctrl => self.diff_pending = Some('['),
            KeyCode::Tab => self.diff_panel = !self.diff_panel,
            // Ctrl-j/k step files, as they always have.
            KeyCode::Char('j') | KeyCode::Down if ctrl => {
                self.diff_goto_file((self.diff_sel + 1).min(nfiles - 1))
            }
            KeyCode::Char('k') | KeyCode::Up if ctrl => {
                self.diff_goto_file(self.diff_sel.saturating_sub(1))
            }
            KeyCode::Char('j') | KeyCode::Down => self.diff_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.diff_scroll = self.diff_scroll.saturating_sub(1)
            }
            KeyCode::Char('d') if ctrl => self.diff_scroll += 12,
            KeyCode::Char('u') if ctrl => {
                self.diff_scroll = self.diff_scroll.saturating_sub(12)
            }
            KeyCode::Char('g') => self.diff_scroll = 0,
            KeyCode::Char('G') => self.diff_scroll = usize::MAX, // clamped in render
            _ => {}
        }
    }

    // ---- ga fuzzy picker ----

    fn open_picker(&mut self) {
        self.picker_query.clear();
        self.picker_sel = 0;
        self.overlay = Overlay::Picker;
        self.mode = Mode::Normal;
    }

    /// Case-insensitive subsequence match over space names; returns (space
    /// index, matched char positions) per hit, busy spaces first.
    pub fn picker_matches(&self) -> Vec<(usize, Vec<usize>)> {
        let q: Vec<char> = self.picker_query.to_lowercase().chars().collect();
        let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, sp) in self.spaces.iter().enumerate() {
            let title = space_name(sp).to_lowercase();
            let mut pos = Vec::new();
            let mut qi = 0;
            for (ci, ch) in title.chars().enumerate() {
                if qi < q.len() && ch == q[qi] {
                    pos.push(ci);
                    qi += 1;
                }
            }
            if qi == q.len() {
                out.push((i, pos));
            }
        }
        out.sort_by_key(|(i, _)| self.spaces[*i].status() != ChatStatus::Running);
        out
    }

    fn key_picker(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let m = self.picker_matches();
                let pick = m
                    .get(self.picker_sel.min(m.len().saturating_sub(1)))
                    .map(|(i, _)| *i);
                self.overlay = Overlay::None;
                if let Some(idx) = pick {
                    self.jump_space(idx);
                }
            }
            KeyCode::Down => self.picker_move(1),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Char('j') | KeyCode::Char('n') if ctrl => self.picker_move(1),
            KeyCode::Char('k') | KeyCode::Char('p') if ctrl => self.picker_move(-1),
            KeyCode::Backspace => {
                let at = self.picker_query.len();
                let start = prev_grapheme(&self.picker_query, at);
                self.picker_query.truncate(start);
                self.picker_sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.picker_query.push(c);
                self.picker_sel = 0;
            }
            _ => {}
        }
    }

    fn picker_move(&mut self, d: isize) {
        let n = self.picker_matches().len();
        if n == 0 {
            return;
        }
        let cur = self.picker_sel.min(n - 1) as isize;
        self.picker_sel = (cur + d).rem_euclid(n as isize) as usize;
    }

    // ---- gd fuzzy directory picker ----

    /// `gd` / bare `:cd`. Scans on open rather than on every keystroke — the
    /// walk is bounded (see `scan_dirs`) but not free.
    fn open_dir_picker(&mut self) {
        self.dir_candidates = scan_dirs();
        self.dir_query.clear();
        self.dir_sel = 0;
        self.overlay = Overlay::DirPicker;
        self.mode = Mode::Normal;
    }

    /// Case-insensitive subsequence match over the `~/…` form, same rule as
    /// the space picker. Shortest paths first, so `~/aeovim` outranks a deep
    /// path that happens to contain the same letters.
    pub fn dir_matches(&self) -> Vec<(usize, Vec<usize>)> {
        let q: Vec<char> = self.dir_query.to_lowercase().chars().collect();
        let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, p) in self.dir_candidates.iter().enumerate() {
            let hay = tilde(p).to_lowercase();
            let mut pos = Vec::new();
            let mut qi = 0;
            for (ci, ch) in hay.chars().enumerate() {
                if qi < q.len() && ch == q[qi] {
                    pos.push(ci);
                    qi += 1;
                }
            }
            if qi == q.len() {
                out.push((i, pos));
            }
        }
        out.sort_by_key(|(i, _)| tilde(&self.dir_candidates[*i]).len());
        out.truncate(200);
        out
    }

    fn key_dir_picker(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let m = self.dir_matches();
                let pick = m
                    .get(self.dir_sel.min(m.len().saturating_sub(1)))
                    .map(|(i, _)| self.dir_candidates[*i].clone());
                self.overlay = Overlay::None;
                if let Some(p) = pick {
                    self.set_space_dir(p);
                }
            }
            KeyCode::Down => self.dir_move(1),
            KeyCode::Up => self.dir_move(-1),
            KeyCode::Char('j') | KeyCode::Char('n') if ctrl => self.dir_move(1),
            KeyCode::Char('k') | KeyCode::Char('p') if ctrl => self.dir_move(-1),
            KeyCode::Backspace => {
                let at = self.dir_query.len();
                let start = prev_grapheme(&self.dir_query, at);
                self.dir_query.truncate(start);
                self.dir_sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.dir_query.push(c);
                self.dir_sel = 0;
            }
            _ => {}
        }
    }

    fn dir_move(&mut self, d: isize) {
        let n = self.dir_matches().len();
        if n == 0 {
            return;
        }
        let cur = self.dir_sel.min(n - 1) as isize;
        self.dir_sel = (cur + d).rem_euclid(n as isize) as usize;
    }

    /// Point this space at `dir`. Live claude children keep the cwd they were
    /// spawned with, so they are killed here: the next turn respawns in the
    /// new directory (`--resume` keeps the conversation). Without this the
    /// header would claim a directory the agent isn't actually working in.
    fn set_space_dir(&mut self, dir: PathBuf) {
        let shown = tilde(&dir);
        let idx = self.active;
        let Some(sp) = self.spaces.get_mut(idx) else { return };
        if sp.dir == dir {
            self.toast = Some(format!("already in {shown}"));
            return;
        }
        // Changing dir kills every child in the space, so a running turn would
        // die and surface as a session error. Make the user interrupt first.
        if sp.chats.iter().any(|c| c.in_flight) {
            self.toast = Some("turn still running — Ctrl-c first, then :cd".into());
            return;
        }
        self.prev_dir = Some(std::mem::replace(&mut sp.dir, dir));
        let mut restarted = 0;
        for c in &mut sp.chats {
            if let Some(s) = c.session.take() {
                s.kill();
                restarted += 1;
            }
        }
        self.persist();
        self.toast = Some(match restarted {
            0 => format!("space dir → {shown}"),
            1 => format!("space dir → {shown} · session restarts on next turn"),
            n => format!("space dir → {shown} · {n} sessions restart on next turn"),
        });
    }

    // ---- task board ----

    fn open_board(&mut self) {
        let rows = self.board_rows();
        self.board_sel = rows
            .iter()
            .position(|(si, _)| *si == self.active)
            .unwrap_or(0);
        self.overlay = Overlay::Board;
        self.mode = Mode::Normal;
    }

    fn key_board(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => self.board_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.board_move(-1),
            KeyCode::Enter => {
                let rows = self.board_rows();
                self.overlay = Overlay::None;
                if let Some((si, ci)) = rows.get(self.board_sel).copied() {
                    self.jump_space(si);
                    self.spaces[si].focused = ci;
                }
            }
            KeyCode::Char('x') => {
                if let Some((si, ci)) = self.board_rows().get(self.board_sel).copied() {
                    self.interrupt_at(si, ci);
                }
            }
            _ => {}
        }
    }

    fn board_move(&mut self, d: isize) {
        let n = self.board_rows().len();
        if n == 0 {
            return;
        }
        let cur = self.board_sel.min(n - 1) as isize;
        self.board_sel = (cur + d).rem_euclid(n as isize) as usize;
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
            // Esc: back to Normal mode, vim-style — the draft stays (Ctrl-u
            // clears it). Interrupt lives on Ctrl-c, and on Esc in Normal.
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
            // Arrows scroll the transcript when there's nothing to edit;
            // otherwise they move the cursor through the draft.
            KeyCode::Up => {
                if self.input.is_empty() {
                    self.scroll_up(1);
                } else {
                    self.cursor_vertical(-1);
                }
            }
            KeyCode::Down => {
                if self.input.is_empty() {
                    self.scroll_down(1);
                } else {
                    self.cursor_vertical(1);
                }
            }
            KeyCode::Home => self.cursor_line_home(),
            KeyCode::End => {
                if self.input.is_empty() {
                    self.chat_mut().follow = true; // jump to the live tail
                } else {
                    self.cursor_line_end();
                }
            }
            KeyCode::F(1) => self.overlay = Overlay::Help,
            KeyCode::Char('a') if ctrl => self.cursor_line_home(),
            KeyCode::Char('e') if ctrl => self.cursor_line_end(),
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.input_cursor = 0;
                self.slash_sel = 0;
            }
            KeyCode::Char('v') if ctrl => self.paste_from_clipboard(),
            KeyCode::Char('t') if ctrl => {
                let c = self.chat_mut();
                c.expand_tools = !c.expand_tools;
            }
            // Ctrl-D: readline delete-char-or-EOF — quits (asks) on an empty
            // composer, deletes under the cursor otherwise.
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.confirm_quit();
                } else {
                    self.delete_at_cursor();
                }
            }
            // Ctrl-C: clear the draft, or interrupt the running turn, or ask to
            // quit — in that order.
            KeyCode::Char('c') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.slash_sel = 0;
                } else if self.chat().in_flight {
                    self.interrupt();
                } else {
                    self.confirm_quit();
                }
            }
            KeyCode::PageUp => self.scroll_up(12),
            KeyCode::PageDown => self.scroll_down(12),
            // `!ctrl` or an unhandled chord (Ctrl-b, Ctrl-x, …) types its bare
            // letter into the composer instead of being ignored.
            KeyCode::Char(c) if !ctrl => {
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

    /// Ask before quitting — quitting used to be zero-friction, which made a
    /// stray keypress catastrophic mid-turn.
    fn confirm_quit(&mut self) {
        let running: usize = self
            .spaces
            .iter()
            .map(|s| s.chats.iter().filter(|c| c.in_flight).count())
            .sum();
        self.confirm_msg = match running {
            0 => "quit aeovim?   y / n".to_string(),
            1 => "quit aeovim? a turn is still running   y / n".to_string(),
            n => format!("quit aeovim? {n} turns are still running   y / n"),
        };
        self.confirm_what = ConfirmWhat::Quit;
        self.mode = Mode::Confirm;
    }

    fn all_slash_commands(&self) -> Vec<String> {
        const DEFAULT: &[&str] = &[
            "init", "review", "security-review", "pr-comments", "compact", "context",
            "cost", "agents", "mcp", "memory", "model", "todos", "help", "clear",
            "rename", "mouse", "tools", "quit",
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
                let what = self.confirm_what;
                self.mode = Mode::Normal;
                match what {
                    ConfirmWhat::Quit => self.should_quit = true,
                    ConfirmWhat::DeleteSpace(i) => self.delete_space(i),
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let c = self.chat_mut();
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
        let c = self.chat_mut();
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

    /// UI-owned slash commands, handled locally instead of being sent to the
    /// agent. Returns true when `prompt` was consumed.
    fn exec_local_slash(&mut self, prompt: &str) -> bool {
        let mut parts = prompt.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match head {
            "/clear" => self.reset_conversation(),
            "/help" | "/keys" => self.overlay = Overlay::Help,
            "/quit" | "/exit" => self.should_quit = true, // explicit — no confirm
            "/new" => self.new_space(),
            "/tasks" | "/board" => self.open_board(),
            "/diff" => self.open_diff_pad(),
            "/rename" => {
                if arg.is_empty() {
                    self.start_rename(self.active);
                } else {
                    self.space_mut().name = arg.to_string();
                    self.persist();
                }
            }
            "/mouse" => match arg {
                "on" => self.set_mouse(true),
                "off" => self.set_mouse(false),
                _ => self.toggle_mouse(),
            },
            "/tools" => {
                let c = self.chat_mut();
                c.expand_tools = !c.expand_tools;
            }
            _ => return false,
        }
        true
    }

    /// Per-space, because each space has its own working directory — telling
    /// the agent the process cwd would name a directory it isn't in.
    fn env_prompt(&self, si: usize) -> String {
        let cwd = self
            .spaces
            .get(si)
            .map(|sp| sp.dir.display().to_string())
            .unwrap_or_default();
        format!(
            "You are running inside aeovim — a keyboard-driven terminal UI that wraps the \
Claude Code CLI. Working directory: {cwd}. You're in a terminal on macOS \
(tmux/Ghostty) — keep output concise and terminal-friendly."
        )
    }

    /// Make sure chat (si, ci) has a live child, spawning one if needed.
    /// Returns false if the spawn failed (an Error entry is pushed).
    fn ensure_session(&mut self, si: usize, ci: usize) -> bool {
        let c = &self.spaces[si].chats[ci];
        if c.session.as_ref().is_some_and(|s| s.is_alive()) {
            return true;
        }
        let spec = crate::agent::SessionSpec {
            chat: c.id,
            session_id: c.session_id.clone(),
            resume: !c.first_turn,
            model: self.model_cli.clone(),
            dangerous: self.dangerous,
            permission_mode: "acceptEdits".into(),
            append_system_prompt: Some(self.env_prompt(si)),
            cwd: self.spaces[si].dir.clone(),
        };
        match crate::agent::spawn_session(spec, self.tx.clone()) {
            Ok(h) => {
                let c = &mut self.spaces[si].chats[ci];
                c.session = Some(h);
                c.first_turn = false;
                true
            }
            Err(e) => {
                self.spaces[si].chats[ci].push(Entry::Error(e.to_string()));
                false
            }
        }
    }

    /// Deliver a prompt as a turn on chat (si, ci). Assumes the user entry
    /// (if any) is already on the transcript — the resume self-heal replays
    /// through here without re-pushing it.
    fn deliver(&mut self, si: usize, ci: usize, prompt: String, snap_to_bottom: bool) {
        if !self.ensure_session(si, ci) {
            return;
        }
        let c = &mut self.spaces[si].chats[ci];
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

    /// Interrupt the focused chat's in-flight turn. First press asks the
    /// child nicely (control protocol — keeps the session alive); a second
    /// press while still interrupting hard-kills the child.
    fn interrupt(&mut self) {
        let si = self.active;
        let ci = self.spaces[si].focused.min(self.spaces[si].chats.len() - 1);
        self.interrupt_at(si, ci);
    }

    fn interrupt_at(&mut self, si: usize, ci: usize) {
        let Some(c) = self.spaces.get_mut(si).and_then(|s| s.chats.get_mut(ci)) else {
            return;
        };
        if !c.in_flight {
            return;
        }
        if c.interrupting {
            if let Some(s) = &c.session {
                s.kill();
            } else {
                // Second press with no child left to kill: nothing will ever
                // send SessionEnded, so clear the turn here or the chat stays
                // in_flight forever with no way out but quitting.
                c.in_flight = false;
                c.interrupting = false;
                c.turn_started = None;
                c.activity = None;
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

    /// Stop every live child (used on quit so nothing runs on invisibly).
    pub fn kill_all_sessions(&mut self) {
        for sp in &mut self.spaces {
            for c in &mut sp.chats {
                if let Some(s) = c.session.take() {
                    s.kill();
                }
            }
        }
    }

    /// /clear · :clear — wipe the focused transcript, fresh claude session.
    fn reset_conversation(&mut self) {
        let c = self.chat_mut();
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
    /// newlines to spaces.
    fn paste(&mut self, raw: String) {
        let text = sanitize_paste(&raw);
        if text.is_empty() {
            return;
        }
        let flat = || text.replace('\n', " ");
        match self.mode {
            Mode::Insert => self.insert_at_cursor(&text),
            Mode::Rename => self.rename_buf.push_str(&flat()),
            Mode::Command => self.cmdline.push_str(&flat()),
            Mode::Normal | Mode::Confirm => {}
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
        // A drafted ex command (":diff", ":vs", …) executes instead of being
        // sent to the model — typing : straight from Insert is too natural a
        // reflex to punish with a confused agent turn.
        if let Some(cmd) = prompt.strip_prefix(':') {
            let cmd = cmd.trim().to_string();
            if !cmd.is_empty() {
                self.input.clear();
                self.input_cursor = 0;
                self.slash_sel = 0;
                self.exec_command(&cmd);
                return;
            }
        }
        if prompt.starts_with('/') {
            let consumed = self.exec_local_slash(&prompt);
            if consumed {
                self.input.clear();
                self.input_cursor = 0;
                self.slash_sel = 0;
                return;
            }
        }
        self.input.clear();
        self.input_cursor = 0;
        self.slash_sel = 0;
        // Busy? Queue it (shown at the bottom) and send it when the turn frees
        // up — turns on one chat are strictly sequential.
        let si = self.active;
        let ci = self.spaces[si].focused.min(self.spaces[si].chats.len() - 1);
        // Name the space off its first prompt — before the queue check, so a
        // prompt that lands while a turn is running still names it.
        if is_default_name(&self.spaces[si].name) {
            self.spaces[si].name = slug(&prompt);
        }
        if self.spaces[si].chats[ci].in_flight {
            let c = &mut self.spaces[si].chats[ci];
            c.queue.push(prompt);
            c.follow = true;
            return;
        }
        {
            let c = &mut self.spaces[si].chats[ci];
            c.push(Entry::User(prompt.clone()));
            c.last_prompt = Some(prompt.clone());
            c.healed_once = false;
        }
        self.deliver(si, ci, prompt, true);
    }

    /// A turn just ended on chat (si, ci) — if prompts were queued while it
    /// ran, send the next one. One at a time; the following turn's end drains
    /// the rest.
    fn drain_queue(&mut self, si: usize, ci: usize) {
        let Some(c) = self.spaces.get(si).and_then(|s| s.chats.get(ci)) else {
            return;
        };
        if c.in_flight || c.queue.is_empty() {
            return;
        }
        let next = self.spaces[si].chats[ci].queue.remove(0);
        {
            let c = &mut self.spaces[si].chats[ci];
            c.push(Entry::User(next.clone()));
            c.last_prompt = Some(next.clone());
            c.healed_once = false;
        }
        self.deliver(si, ci, next, false);
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
    fn inner_change_isolates_the_changed_run() {
        // Common prefix and suffix are trimmed; only the middle is reported.
        let (d, a) = inner_change("let x = old_value();", "let x = new_value();").unwrap();
        assert_eq!(&"let x = old_value();"[d.0..d.1], "old");
        assert_eq!(&"let x = new_value();"[a.0..a.1], "new");
        // Identical lines have no inner change at all.
        assert!(inner_change("same", "same").is_none());
        // A pure append reports only the appended tail.
        let (d, a) = inner_change("foo", "foobar").unwrap();
        assert_eq!(d.0, d.1, "nothing removed");
        assert_eq!(&"foobar"[a.0..a.1], "bar");
    }

    #[test]
    fn inner_change_never_splits_a_multibyte_char() {
        // Byte ranges that landed mid-codepoint used to panic the renderer the
        // moment it sliced on them.
        for (a, b) in [("héllo wörld", "héllo wérld"), ("→x", "→y"), ("🙂a", "🙂b")] {
            let (ra, rb) = inner_change(a, b).unwrap();
            assert!(a.is_char_boundary(ra.0) && a.is_char_boundary(ra.1));
            assert!(b.is_char_boundary(rb.0) && b.is_char_boundary(rb.1));
            // And the ranges must actually be sliceable.
            let _ = (&a[ra.0..ra.1], &b[rb.0..rb.1]);
        }
    }

    #[test]
    fn annotate_numbers_the_new_side_and_pairs_changes() {
        let (_, _, lines) = line_diff("a\nold\nc", "a\nnew\nc");
        let ann = annotate(&lines);
        assert_eq!(lines.len(), ann.len());
        for (l, (num, hl)) in lines.iter().zip(&ann) {
            match l.kind {
                // Deletions are unnumbered virtual rows, as in codediff.
                DiffKind::Del => {
                    assert!(num.is_none(), "deletion must not be numbered");
                    assert!(hl.is_some(), "old→new pair must carry an inner change");
                }
                DiffKind::Add => {
                    assert!(num.is_some(), "addition must be numbered");
                    assert!(hl.is_some());
                }
                _ => {}
            }
        }
        // Context and additions number the new side consecutively: a=1, new=2, c=3.
        let nums: Vec<u32> = ann.iter().filter_map(|(n, _)| *n).collect();
        assert_eq!(nums, vec![1, 2, 3]);
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
        let mut c = Chat::blank(1, "sid".into(), true, 0.0);
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
        let mut c = Chat::blank(1, "sid".into(), true, 0.0);
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

    fn pchat(sid: &str) -> PersistChat {
        PersistChat {
            title: String::new(),
            session_id: sid.into(),
            cost: 0.0,
            started: true,
            transcript: vec![],
        }
    }

    fn app_with(restored: Vec<PersistSpace>) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(None, false, tx, "selftest".into(), restored)
    }

    /// Launching in a directory reopens the space already pointed at it,
    /// rather than whatever space happened to be active on exit.
    #[test]
    fn launch_dir_reopens_that_dirs_space() {
        let here = std::env::current_dir().unwrap();
        let app = app_with(vec![
            PersistSpace {
                name: "elsewhere".into(),
                dir: Some("/tmp".into()),
                chats: vec![pchat("a")],
            },
            PersistSpace {
                name: "this one".into(),
                dir: Some(here.display().to_string()),
                chats: vec![pchat("b")],
            },
        ]);
        assert_eq!(app.space().name, "this one");
        assert_eq!(app.space().dir, here);
    }

    /// A directory with no saved space gets a fresh one, and the saved spaces
    /// are still there to switch back to.
    #[test]
    fn launch_dir_without_a_space_creates_one() {
        let here = std::env::current_dir().unwrap();
        let app = app_with(vec![PersistSpace {
            name: "elsewhere".into(),
            dir: Some("/tmp".into()),
            chats: vec![pchat("a")],
        }]);
        assert_eq!(app.spaces.len(), 2);
        assert_eq!(app.space().dir, here);
        assert!(app.space().name.trim().is_empty(), "fresh space is unnamed");
        assert_eq!(app.spaces[0].dir, std::path::PathBuf::from("/tmp"));
    }
}
