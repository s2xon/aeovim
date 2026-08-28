//! App state + update logic — two-level model.
//!
//! A **Space** is a named container of 1–4 **Chats**. The sidebar lists spaces;
//! the active space renders its chats as split panes. Every chat belongs to
//! exactly one space; deleting a space's last chat deletes the space. Spaces can
//! be merged (chats combined, ≤4) and a chat can be popped out into its own space.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use serde_json::Value;

use crate::agent::{spawn_turn, TurnSpec};
use crate::protocol::AgentEvent;
use crate::store::{self, PersistChat, PersistSpace};

#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Rename,
    Picker,
    Confirm,
    Search,
}

#[derive(PartialEq, Clone, Copy)]
pub enum ConfirmAction {
    DeleteSpaces,
    Quit,
}

/// Something happened in a chat while its space wasn't on screen.
#[derive(PartialEq, Clone, Copy)]
pub enum Attn {
    Done,
    Error,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(PartialEq, Clone, Copy)]
pub enum RenameTarget {
    Space,
    Chat,
}

#[derive(PartialEq, Clone, Copy)]
pub enum SplitDir {
    V,
    H,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Pending {
    None,
    G,
    Z,
    Leader,
    LeaderE,
    LeaderN,
    LeaderS,
    LeaderT,
    LeaderZ,
}

#[derive(Clone, Copy)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

pub enum Entry {
    User(String),
    Assistant(String),
    Tool(String),
    ToolResult { ok: bool, text: String },
    Note(String),
    Error(String),
}

pub struct Chat {
    pub id: u64,
    pub title: String,
    pub autonamed: bool,
    pub transcript: Vec<Entry>,
    pub streaming: Option<String>,
    /// Streaming extended-thinking text (tail only; shown dim while working).
    pub thinking: Option<String>,
    pub in_flight: bool,
    pub session_id: String,
    pub first_turn: bool,
    pub cost: f64,
    /// Input-side tokens of the last turn ≈ context size.
    pub context_tokens: u64,
    pub scroll: u16,
    pub follow: bool,
    pub last_max_scroll: u16,
    /// Body width at last render — lets search wrap lines exactly like the UI.
    pub last_body_width: u16,
    /// `za`: show tool results in full instead of one-line summaries.
    pub expand_tools: bool,
    /// Per-chat model override (`:model`).
    pub model: Option<String>,
    pub attention: Option<Attn>,
    /// Prompts waiting for the current turn to finish (type-ahead, pipe, :all).
    pub queued: Vec<String>,
    last_prompt: String,
    session_retry: bool,
}

impl Chat {
    fn fresh(id: u64) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let mut transcript = Vec::new();
        transcript.push(Entry::Note(format!("session {}", &session_id[..8])));
        Chat {
            id,
            title: String::new(),
            autonamed: false,
            transcript,
            streaming: None,
            thinking: None,
            in_flight: false,
            session_id,
            first_turn: true,
            cost: 0.0,
            context_tokens: 0,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
            last_body_width: 0,
            expand_tools: false,
            model: None,
            attention: None,
            queued: Vec::new(),
            last_prompt: String::new(),
            session_retry: false,
        }
    }

    fn from_persist(id: u64, pc: &PersistChat, cwd: &str) -> Self {
        let mut transcript = replay_transcript(cwd, &pc.session_id);
        let short = &pc.session_id[..8.min(pc.session_id.len())];
        let note = if transcript.is_empty() {
            format!("resumed · session {short} (send a message to continue)")
        } else {
            format!("resumed · session {short} · history restored")
        };
        transcript.push(Entry::Note(note));
        Chat {
            id,
            title: pc.title.clone(),
            autonamed: true,
            transcript,
            streaming: None,
            thinking: None,
            in_flight: false,
            session_id: pc.session_id.clone(),
            first_turn: false,
            cost: pc.cost,
            context_tokens: 0,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
            last_body_width: 0,
            expand_tools: false,
            model: pc.model.clone(),
            attention: None,
            queued: Vec::new(),
            last_prompt: String::new(),
            session_retry: false,
        }
    }

    fn commit_streaming(&mut self) {
        if let Some(s) = self.streaming.take() {
            if !s.trim().is_empty() {
                self.transcript.push(Entry::Assistant(s));
            }
        }
    }
}

pub struct Space {
    pub id: u64,
    pub name: String,
    pub chats: Vec<Chat>,
    pub focused: usize,
    pub split_dir: SplitDir,
    pub zoom: bool,
    /// Working directory for this space's agents (`:cd`); None = launch cwd.
    pub cwd: Option<String>,
}

impl Space {
    fn one(id: u64, chat: Chat) -> Self {
        Space {
            id,
            name: String::new(),
            chats: vec![chat],
            focused: 0,
            split_dir: SplitDir::V,
            zoom: false,
            cwd: None,
        }
    }
    pub fn fi(&self) -> usize {
        self.focused.min(self.chats.len().saturating_sub(1))
    }
}

pub fn chat_title(c: &Chat) -> String {
    if c.title.trim().is_empty() {
        "untitled".to_string()
    } else {
        c.title.clone()
    }
}

/// Display name for a space: its name, else (single chat) the chat's title.
pub fn space_name(sp: &Space) -> String {
    if !sp.name.trim().is_empty() {
        sp.name.clone()
    } else {
        "space".to_string()
    }
}

fn slug(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.trim().is_empty() {
        return "chat".into();
    }
    let mut out: String = one_line.chars().take(28).collect();
    if one_line.chars().count() > 28 {
        out.push('…');
    }
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
/// terminal and corrupt neighbouring panes). Tabs → space.
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
                _ => {
                    it.next();
                }
            },
            '\t' => out.push(' '),
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

/// Compact a tool result to one clean line (+N lines) for the transcript.
pub fn tool_result_summary(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .copied()
        .unwrap_or("");
    let clean = truncate_str(first, 72);
    let extra = lines.len().saturating_sub(1);
    if extra > 0 {
        format!("{clean}  (+{extra} lines)")
    } else {
        clean
    }
}

/// Format a tool call the way Claude Code shows it: ⏺ Tool(arg) + a summary.
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

/// Rebuild a chat's transcript from Claude Code's stored session JSONL
/// (`~/.claude/projects/...`). Best-effort: unknown lines are skipped.
fn replay_transcript(cwd: &str, session_id: &str) -> Vec<Entry> {
    let Some(data) = store::session_transcript(cwd, session_id) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = Vec::new();
    let push_unique = |out: &mut Vec<Entry>, e: Entry| {
        // session files can repeat a message (one line per content block)
        let dup = match (&e, out.last()) {
            (Entry::User(a), Some(Entry::User(b))) => a == b,
            (Entry::Assistant(a), Some(Entry::Assistant(b))) => a == b,
            _ => false,
        };
        if !dup {
            out.push(e);
        }
    };
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
            || v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
        {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
                    continue;
                };
                if let Some(s) = content.as_str() {
                    if !looks_meta(s) && !s.trim().is_empty() {
                        push_unique(&mut out, Entry::User(s.to_string()));
                    }
                } else if let Some(arr) = content.as_array() {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                                if !looks_meta(t) && !t.trim().is_empty() {
                                    push_unique(&mut out, Entry::User(t.to_string()));
                                }
                            }
                            Some("tool_result") => {
                                let ok = !b
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                let text = b
                                    .get("content")
                                    .map(crate::protocol::tool_result_text)
                                    .unwrap_or_default();
                                if !text.trim().is_empty() {
                                    out.push(Entry::ToolResult { ok, text });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("assistant") => {
                let Some(arr) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                let mut text = String::new();
                for b in arr {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            text.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        Some("tool_use") => {
                            if !text.trim().is_empty() {
                                push_unique(&mut out, Entry::Assistant(std::mem::take(&mut text)));
                            }
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            out.push(Entry::Tool(format_tool(name, &input)));
                        }
                        _ => {}
                    }
                }
                if !text.trim().is_empty() {
                    push_unique(&mut out, Entry::Assistant(text));
                }
            }
            _ => {}
        }
    }
    // keep only the tail — giant sessions would bloat memory and first paint
    const MAX: usize = 400;
    if out.len() > MAX {
        let cut = out.len() - MAX;
        out.drain(..cut);
        out.insert(0, Entry::Note(format!("… {cut} earlier entries omitted")));
    }
    out
}

/// Local-command echoes and system reminders stored in session files that
/// aren't real conversation turns.
fn looks_meta(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("<local-command")
        || t.starts_with("<command-")
        || t.starts_with("Caveat:")
        || t.starts_with("<system-reminder")
}

fn expand_home(p: &str) -> String {
    if p == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| p.to_string());
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return format!("{h}/{rest}");
        }
    }
    p.to_string()
}

fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Content of the last fenced code block in `text`, if any.
fn last_code_block(text: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for l in text.lines() {
        if l.trim_start().starts_with("```") {
            match cur.take() {
                Some(b) => blocks.push(b),
                None => cur = Some(String::new()),
            }
        } else if let Some(b) = cur.as_mut() {
            b.push_str(l);
            b.push('\n');
        }
    }
    blocks.pop()
}

fn copy_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

pub enum Msg {
    Input(Event),
    Tick,
    Agent { chat: u64, ev: AgentEvent },
    TurnEnded { chat: u64, error: Option<String> },
    Pipe { to: String, from: String, message: String },
}

pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub pending: Pending,
    pub input: String,
    /// Cursor position in `input`, as a char offset.
    pub input_cursor: usize,
    pub cmd: String,
    pub search_buf: String,
    pub search_query: String,
    /// Transient one-line status (yank feedback, search misses, :cmd results).
    pub flash: String,
    pub rename_buf: String,
    pub rename_target: RenameTarget,
    pub picker_query: String,
    pub picker_sel: usize,
    pub spaces: Vec<Space>,
    pub active_space: usize,
    pub sidebar_cursor: usize,
    pub sidebar_open: bool,
    selected: Vec<u64>,
    pending_delete: Vec<u64>,
    pub confirm_msg: String,
    confirm_action: ConfirmAction,
    pub model_cli: Option<String>,
    pub model_display: String,
    pub dangerous: bool,
    pub should_quit: bool,
    pub spinner: usize,
    pub help_open: bool,
    pub slash_commands: Vec<String>,
    pub slash_sel: usize,
    next_chat_id: u64,
    next_space_id: u64,
    chat_counter: u64,
    space_counter: u64,
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
        let mut spaces = Vec::new();
        let mut next_chat_id = 1u64;
        let mut next_space_id = 1u64;
        let chat_counter = 1u64;
        let space_counter = 1u64;

        let launch_cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        for ps in &restored {
            let cwd = ps.cwd.clone().unwrap_or_else(|| launch_cwd.clone());
            let mut chats: Vec<Chat> = Vec::new();
            for pc in ps.chats.iter().take(4) {
                chats.push(Chat::from_persist(next_chat_id, pc, &cwd));
                next_chat_id += 1;
            }
            if chats.is_empty() {
                continue;
            }
            spaces.push(Space {
                id: next_space_id,
                name: ps.name.clone(),
                chats,
                focused: 0,
                split_dir: SplitDir::V,
                zoom: false,
                cwd: ps.cwd.clone(),
            });
            next_space_id += 1;
        }
        // No spaces (deleted-all, or first run) → stay empty and show the
        // "start a space" state; do not auto-create a default space.

        Self {
            mode: Mode::Normal,
            focus: Focus::Main,
            pending: Pending::None,
            input: String::new(),
            input_cursor: 0,
            cmd: String::new(),
            search_buf: String::new(),
            search_query: String::new(),
            flash: String::new(),
            rename_buf: String::new(),
            rename_target: RenameTarget::Space,
            picker_query: String::new(),
            picker_sel: 0,
            spaces,
            active_space: 0,
            sidebar_cursor: 0,
            sidebar_open: true,
            selected: Vec::new(),
            pending_delete: Vec::new(),
            confirm_msg: String::new(),
            confirm_action: ConfirmAction::DeleteSpaces,
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            help_open: false,
            slash_commands: Vec::new(),
            slash_sel: 0,
            next_chat_id,
            next_space_id,
            chat_counter,
            space_counter,
            workspace_key,
            tx,
        }
    }

    // ---- lookups ----

    pub fn cur_chat(&self) -> &Chat {
        let sp = &self.spaces[self.active_space];
        &sp.chats[sp.fi()]
    }
    fn cur_chat_mut(&mut self) -> &mut Chat {
        let ai = self.active_space;
        let fi = self.spaces[ai].fi();
        &mut self.spaces[ai].chats[fi]
    }
    fn chat_by_id_mut(&mut self, id: u64) -> Option<&mut Chat> {
        for sp in &mut self.spaces {
            for c in &mut sp.chats {
                if c.id == id {
                    return Some(c);
                }
            }
        }
        None
    }
    fn chat_pos(&self, id: u64) -> Option<(usize, usize)> {
        for (si, sp) in self.spaces.iter().enumerate() {
            for (ci, c) in sp.chats.iter().enumerate() {
                if c.id == id {
                    return Some((si, ci));
                }
            }
        }
        None
    }
    fn space_index(&self, id: u64) -> Option<usize> {
        self.spaces.iter().position(|s| s.id == id)
    }
    pub fn sel_space_id(&self) -> Option<u64> {
        self.spaces.get(self.sidebar_cursor).map(|s| s.id)
    }
    pub fn is_selected(&self, id: u64) -> bool {
        self.selected.contains(&id)
    }
    pub fn any_in_flight(&self) -> bool {
        self.spaces
            .iter()
            .any(|sp| sp.chats.iter().any(|c| c.in_flight))
    }

    pub fn persist(&self) {
        let data: Vec<PersistSpace> = self
            .spaces
            .iter()
            .map(|sp| PersistSpace {
                name: sp.name.clone(),
                cwd: sp.cwd.clone(),
                chats: sp
                    .chats
                    .iter()
                    .map(|c| PersistChat {
                        title: c.title.clone(),
                        session_id: c.session_id.clone(),
                        cost: c.cost,
                        model: c.model.clone(),
                    })
                    .collect(),
            })
            .collect();
        store::save(&self.workspace_key, &data);
    }

    // ---- events ----

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => self.spinner = self.spinner.wrapping_add(1),
            Msg::Input(Event::Key(k)) => self.handle_key(k),
            Msg::Input(Event::Paste(s)) => self.handle_paste(s),
            Msg::Input(_) => {}
            Msg::Pipe { to, from, message } => self.inject_pipe(to, from, message),
            Msg::Agent { chat, ev } => self.handle_agent(chat, ev),
            Msg::TurnEnded { chat, error } => self.turn_ended(chat, error),
        }
    }

    fn turn_ended(&mut self, chat: u64, error: Option<String>) {
        let pos = self.chat_pos(chat);
        let mut respawn: Option<String> = None;
        let mut had_error = false;
        if let Some(c) = self.chat_by_id_mut(chat) {
            if c.in_flight {
                c.commit_streaming();
                c.in_flight = false;
                c.follow = true;
            }
            c.thinking = None;
            if let Some(e) = error {
                had_error = true;
                // a crash between spawning turn one and persisting can leave a
                // stale --session-id; mint a fresh id and retry the prompt once
                let el = e.to_lowercase();
                if !c.session_retry
                    && el.contains("session")
                    && (el.contains("already in use") || el.contains("already exists"))
                {
                    c.session_retry = true;
                    c.session_id = Uuid::new_v4().to_string();
                    c.first_turn = true;
                    if !c.last_prompt.is_empty() {
                        c.queued.insert(0, c.last_prompt.clone());
                    }
                    c.transcript.push(Entry::Note(
                        "session id collided — retrying with a fresh session".into(),
                    ));
                } else {
                    c.transcript.push(Entry::Error(e));
                }
            } else {
                c.session_retry = false;
            }
            if !c.queued.is_empty() {
                respawn = Some(c.queued.remove(0));
            }
        }
        if let Some((si, ci)) = pos {
            if si != self.active_space {
                self.spaces[si].chats[ci].attention =
                    Some(if had_error { Attn::Error } else { Attn::Done });
            }
            if let Some(prompt) = respawn {
                self.spawn_for(si, ci, prompt);
            }
        }
        self.persist();
    }

    fn handle_agent(&mut self, id: u64, ev: AgentEvent) {
        let is_result = matches!(ev, AgentEvent::TurnResult { .. });
        let mut model_update = None;
        let mut slash_update = None;
        if let Some(c) = self.chat_by_id_mut(id) {
            match ev {
                AgentEvent::Init { session_id, model, slash_commands } => {
                    if let Some(s) = session_id {
                        c.session_id = s;
                    }
                    if let Some(m) = model {
                        model_update = Some(m);
                    }
                    if !slash_commands.is_empty() {
                        slash_update = Some(slash_commands);
                    }
                }
                AgentEvent::TextDelta(s) => {
                    c.follow = true;
                    c.thinking = None;
                    c.streaming.get_or_insert_with(String::new).push_str(&s);
                }
                AgentEvent::ThinkingDelta(s) => {
                    c.follow = true;
                    let t = c.thinking.get_or_insert_with(String::new);
                    t.push_str(&s);
                    // keep a bounded tail — only the last line is rendered
                    if t.len() > 4000 {
                        let mut cut = t.len() - 2000;
                        while cut < t.len() && !t.is_char_boundary(cut) {
                            cut += 1;
                        }
                        *t = t[cut..].to_string();
                    }
                }
                AgentEvent::AssistantFinal(s) => {
                    if c.streaming.as_ref().map_or(true, |x| x.trim().is_empty()) {
                        c.streaming = Some(s);
                    }
                }
                AgentEvent::ToolCall { name, input } => {
                    c.commit_streaming();
                    c.thinking = None;
                    c.transcript.push(Entry::Tool(format_tool(&name, &input)));
                    c.follow = true;
                }
                AgentEvent::ToolResult { ok, text } => {
                    // store the full text; the UI folds it to one line unless
                    // the chat's `za` expand toggle is on
                    if !text.trim().is_empty() {
                        c.transcript.push(Entry::ToolResult { ok, text });
                        c.follow = true;
                    }
                }
                AgentEvent::TurnResult { cost_usd, is_error, text, context_tokens } => {
                    if c.streaming.as_ref().map_or(true, |x| x.trim().is_empty()) {
                        if let Some(t) = text {
                            if !t.trim().is_empty() {
                                c.streaming = Some(t);
                            }
                        }
                    }
                    c.commit_streaming();
                    c.thinking = None;
                    c.cost += cost_usd;
                    if context_tokens > 0 {
                        c.context_tokens = context_tokens;
                    }
                    c.in_flight = false;
                    if is_error {
                        c.transcript.push(Entry::Error("turn ended with error".into()));
                    } else {
                        c.session_retry = false;
                    }
                    c.follow = true;
                }
            }
        }
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(m) = model_update {
            self.model_display = m;
        }
        if let Some(sc) = slash_update {
            self.slash_commands = sc;
        }
        if is_result {
            self.persist();
        }
    }

    // ---- input ----

    fn handle_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.flash.clear();
        if self.help_open {
            self.help_open = false;
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Command => self.key_command(k, ctrl),
            Mode::Rename => self.key_rename(k, ctrl),
            Mode::Insert => self.key_insert(k, ctrl),
            Mode::Picker => self.key_picker(k, ctrl),
            Mode::Confirm => self.key_confirm(k),
            Mode::Search => self.key_search(k, ctrl),
            Mode::Normal => {
                if self.pending != Pending::None {
                    self.handle_pending(k);
                    return;
                }
                self.key_normal(k, ctrl);
            }
        }
    }

    fn handle_paste(&mut self, s: String) {
        let s = s.replace('\r', "\n");
        match self.mode {
            Mode::Insert => {
                let b = byte_at(&self.input, self.input_cursor);
                self.input.insert_str(b, &s);
                self.input_cursor += s.chars().count();
                self.slash_sel = 0;
            }
            Mode::Command => self.cmd.push_str(s.replace('\n', " ").trim_end()),
            Mode::Rename => self.rename_buf.push_str(s.replace('\n', " ").trim_end()),
            Mode::Picker => self.picker_query.push_str(s.replace('\n', " ").trim_end()),
            Mode::Search => self.search_buf.push_str(s.replace('\n', " ").trim_end()),
            _ => {}
        }
    }

    // ---- composer editing (char-offset cursor into `input`) ----

    fn input_insert(&mut self, c: char) {
        let b = byte_at(&self.input, self.input_cursor);
        self.input.insert(b, c);
        self.input_cursor += 1;
    }

    fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let b = byte_at(&self.input, self.input_cursor - 1);
        self.input.remove(b);
        self.input_cursor -= 1;
    }

    fn input_delete_word(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.input_cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = byte_at(&self.input, i);
        let end = byte_at(&self.input, self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor = i;
    }

    fn key_command(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => {
                self.cmd.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.exec_command(),
            KeyCode::Backspace => {
                self.cmd.pop();
            }
            KeyCode::Char('c') if ctrl => {
                self.cmd.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Char(c) => self.cmd.push(c),
            _ => {}
        }
    }

    fn key_rename(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => self.rename_commit(),
            KeyCode::Backspace => {
                self.rename_buf.pop();
            }
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Char('u') if ctrl => self.rename_buf.clear(),
            KeyCode::Char(c) => self.rename_buf.push(c),
            _ => {}
        }
    }

    fn key_insert(&mut self, k: KeyEvent, ctrl: bool) {
        // slash-command popup navigation (when typing "/..." with no space yet)
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
                KeyCode::Tab | KeyCode::Enter => {
                    self.slash_complete();
                    return;
                }
                _ => {}
            }
        }
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter if alt || shift => self.input_insert('\n'),
            KeyCode::Enter => self.send_prompt(),
            KeyCode::Backspace => {
                self.input_backspace();
                self.slash_sel = 0;
            }
            KeyCode::Left => self.input_cursor = self.input_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.input_cursor = (self.input_cursor + 1).min(self.input.chars().count())
            }
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input.chars().count(),
            KeyCode::Char('a') if ctrl => self.input_cursor = 0,
            KeyCode::Char('e') if ctrl => self.input_cursor = self.input.chars().count(),
            KeyCode::Char('w') if ctrl => {
                self.input_delete_word();
                self.slash_sel = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.input_cursor = 0;
                self.slash_sel = 0;
            }
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Char(c) if !ctrl => {
                self.input_insert(c);
                self.slash_sel = 0;
            }
            _ => {}
        }
    }

    fn key_search(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => {
                self.search_buf.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Char('c') if ctrl => {
                self.search_buf.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                self.search_query = self.search_buf.trim().to_string();
                self.search_buf.clear();
                self.mode = Mode::Normal;
                if !self.search_query.is_empty() {
                    self.search_jump(1);
                }
            }
            KeyCode::Backspace => {
                self.search_buf.pop();
            }
            KeyCode::Char('u') if ctrl => self.search_buf.clear(),
            KeyCode::Char(c) if !ctrl => self.search_buf.push(c),
            _ => {}
        }
    }

    /// `n` / `N` — jump to the next/prev transcript line matching the search
    /// query, using the same wrapped lines the renderer produced.
    fn search_jump(&mut self, dir: isize) {
        if self.search_query.is_empty() || self.spaces.is_empty() {
            return;
        }
        let q = self.search_query.clone();
        let case_sensitive = q.chars().any(|c| c.is_uppercase());
        let ql = q.to_lowercase();
        let (width, cur) = {
            let c = self.cur_chat();
            let cur = if c.follow {
                c.last_max_scroll as usize
            } else {
                c.scroll as usize
            };
            (c.last_body_width, cur)
        };
        if width == 0 {
            return;
        }
        let matches: Vec<usize> = crate::ui::chat_lines(self.cur_chat(), 0, width)
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let text = crate::ui::line_text(l);
                if case_sensitive {
                    text.contains(&q)
                } else {
                    text.to_lowercase().contains(&ql)
                }
            })
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            self.flash = format!("no match: {q}");
            return;
        }
        let target = if dir > 0 {
            *matches.iter().find(|&&m| m > cur).unwrap_or(&matches[0])
        } else {
            *matches.iter().rev().find(|&&m| m < cur).unwrap_or(matches.last().unwrap())
        };
        let nth = matches.iter().position(|&m| m == target).unwrap_or(0) + 1;
        let c = self.cur_chat_mut();
        c.follow = false;
        c.scroll = target.min(u16::MAX as usize) as u16;
        self.flash = format!("match {nth}/{}", matches.len());
    }

    /// `y` / `Y` — copy the last assistant reply (or its last code block).
    fn yank_last(&mut self, code_block: bool) {
        if self.focus != Focus::Main || self.spaces.is_empty() {
            return;
        }
        let c = self.cur_chat();
        let Some(text) = c.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(t) => Some(t.clone()),
            _ => None,
        }) else {
            self.flash = "nothing to yank".into();
            return;
        };
        let out = if code_block {
            match last_code_block(&text) {
                Some(b) => b,
                None => {
                    self.flash = "no code block in last reply".into();
                    return;
                }
            }
        } else {
            text
        };
        match copy_clipboard(&out) {
            Ok(()) => {
                self.flash = format!(
                    "yanked {}{} chars",
                    if code_block { "code block, " } else { "" },
                    out.chars().count()
                )
            }
            Err(e) => self.flash = format!("yank failed: {e}"),
        }
    }

    fn request_quit(&mut self) {
        let n = self
            .spaces
            .iter()
            .flat_map(|s| &s.chats)
            .filter(|c| c.in_flight)
            .count();
        if n == 0 {
            self.should_quit = true;
            return;
        }
        self.confirm_msg = if n == 1 {
            "an agent is still running — quit?   y / n".into()
        } else {
            format!("{n} agents still running — quit?   y / n")
        };
        self.confirm_action = ConfirmAction::Quit;
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
            self.input_cursor = self.input.chars().count();
        }
        self.slash_sel = 0;
    }

    fn key_picker(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => self.picker_commit(),
            KeyCode::Backspace => {
                self.picker_query.pop();
                self.picker_sel = 0;
            }
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Down => self.picker_down(),
            KeyCode::Up => self.picker_up(),
            KeyCode::Char('j') if ctrl => self.picker_down(),
            KeyCode::Char('n') if ctrl => self.picker_down(),
            KeyCode::Char('k') if ctrl => self.picker_up(),
            KeyCode::Char('p') if ctrl => self.picker_up(),
            KeyCode::Char(c) => {
                self.picker_query.push(c);
                self.picker_sel = 0;
            }
            _ => {}
        }
    }

    fn key_confirm(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match self.confirm_action {
                ConfirmAction::Quit => self.should_quit = true,
                ConfirmAction::DeleteSpaces => {
                    let ids = std::mem::take(&mut self.pending_delete);
                    for id in ids {
                        if let Some(i) = self.space_index(id) {
                            self.delete_space_at(i);
                        }
                    }
                    self.selected.clear();
                    self.mode = Mode::Normal;
                    self.persist();
                }
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_delete.clear();
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn key_normal(&mut self, k: KeyEvent, ctrl: bool) {
        if self.spaces.is_empty() {
            match k.code {
                KeyCode::Char('n') | KeyCode::Char('i') => {
                    self.new_space();
                    self.mode = Mode::Insert;
                }
                KeyCode::Char('a') => self.new_named_space(),
                KeyCode::Char('e') => self.sidebar_open = !self.sidebar_open,
                KeyCode::Char(':') => {
                    self.cmd.clear();
                    self.mode = Mode::Command;
                }
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('c') if ctrl => self.should_quit = true,
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('c') if ctrl => self.request_quit(),
            KeyCode::Char('h') if ctrl => self.focus_dir(Dir::Left),
            KeyCode::Char('l') if ctrl => self.focus_dir(Dir::Right),
            KeyCode::Char('j') if ctrl => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(1)
                } else {
                    self.focus_dir(Dir::Down)
                }
            }
            KeyCode::Char('k') if ctrl => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(-1)
                } else {
                    self.focus_dir(Dir::Up)
                }
            }
            KeyCode::Char('e') if ctrl => self.focus_sidebar(),
            KeyCode::Char('d') if ctrl => self.scroll_down(8),
            KeyCode::Char('u') if ctrl => self.scroll_up(8),
            KeyCode::Char(' ') => self.pending = Pending::Leader,
            KeyCode::Char(':') => {
                self.cmd.clear();
                self.mode = Mode::Command;
            }
            KeyCode::Char('i') => {
                self.focus = Focus::Main;
                self.mode = Mode::Insert;
                self.cur_chat_mut().follow = true;
            }
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('H') => self.pane_cycle(-1),
            KeyCode::Char('L') => self.pane_cycle(1),
            KeyCode::Char('g') => self.pending = Pending::G,
            KeyCode::Char('z') => self.pending = Pending::Z,
            KeyCode::Char('G') => self.cur_chat_mut().follow = true,
            KeyCode::Char('y') if self.focus == Focus::Main => self.yank_last(false),
            KeyCode::Char('Y') if self.focus == Focus::Main => self.yank_last(true),
            KeyCode::Char('/') if self.focus == Focus::Main => {
                self.search_buf.clear();
                self.mode = Mode::Search;
            }
            KeyCode::Esc => self.search_query.clear(),
            KeyCode::Char('n') => {
                // after a `/` search, n navigates matches; Esc clears the
                // search and restores n = new space
                if self.focus == Focus::Main && !self.search_query.is_empty() {
                    self.search_jump(1);
                } else {
                    self.new_space();
                    self.focus = Focus::Main;
                    self.mode = Mode::Insert;
                }
            }
            KeyCode::Char('N') if self.focus == Focus::Main => {
                if !self.search_query.is_empty() {
                    self.search_jump(-1);
                }
            }
            KeyCode::Char('a') if self.focus == Focus::Sidebar => self.new_named_space(),
            KeyCode::Char('r') => self.rename_start(),
            KeyCode::Char('s') if self.focus == Focus::Sidebar => self.toggle_select(),
            KeyCode::Char('m') if self.focus == Focus::Sidebar => self.merge_selected(),
            KeyCode::Char('d') if self.focus == Focus::Sidebar => self.request_delete(),
            KeyCode::Char('}') => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(5)
                } else {
                    self.scroll_down(10)
                }
            }
            KeyCode::Char('{') => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(-5)
                } else {
                    self.scroll_up(10)
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(1)
                } else {
                    self.scroll_down(1)
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(-1)
                } else {
                    self.scroll_up(1)
                }
            }
            KeyCode::Left => self.focus_dir(Dir::Left),
            KeyCode::Right => {
                if self.focus == Focus::Sidebar {
                    self.activate_selected()
                } else {
                    self.focus_dir(Dir::Right)
                }
            }
            KeyCode::Enter => {
                // Enter only activates a space from the sidebar; no-op in a pane
                if self.focus == Focus::Sidebar {
                    self.activate_selected()
                }
            }
            KeyCode::Tab => self.pane_cycle(1),
            KeyCode::BackTab => self.pane_cycle(-1),
            _ => {}
        }
    }

    fn handle_pending(&mut self, k: KeyEvent) {
        match self.pending {
            Pending::G => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('g') => {
                        let c = self.cur_chat_mut();
                        c.follow = false;
                        c.scroll = 0;
                    }
                    KeyCode::Char('t') => self.pane_cycle(1),
                    KeyCode::Char('T') => self.pane_cycle(-1),
                    _ => {}
                }
            }
            Pending::Z => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('z') => {
                        // recenter on the newest activity (the working line)
                        self.cur_chat_mut().follow = true;
                    }
                    KeyCode::Char('a') => {
                        // fold-toggle: expand/collapse tool results
                        let c = self.cur_chat_mut();
                        c.expand_tools = !c.expand_tools;
                    }
                    _ => {}
                }
            }
            Pending::Leader => match k.code {
                KeyCode::Char('e') => self.pending = Pending::LeaderE,
                KeyCode::Char('z') => self.pending = Pending::LeaderZ,
                KeyCode::Char('n') => self.pending = Pending::LeaderN,
                KeyCode::Char('s') => self.pending = Pending::LeaderS,
                KeyCode::Char('t') => self.pending = Pending::LeaderT,
                KeyCode::Char('a') => {
                    self.pending = Pending::None;
                    self.new_named_space();
                }
                KeyCode::Char(d @ '0'..='9') => {
                    self.pending = Pending::None;
                    self.leader_jump(d);
                }
                _ => self.pending = Pending::None,
            },
            Pending::LeaderS => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('c') => self.open_picker(),
                    KeyCode::Char('n') => self.add_chat_to_active(),
                    KeyCode::Char('p') => self.pop_chat(),
                    KeyCode::Char('x') => self.close_focused_pane(),
                    KeyCode::Char('v') => self.spaces[self.active_space].split_dir = SplitDir::V,
                    KeyCode::Char('h') => self.spaces[self.active_space].split_dir = SplitDir::H,
                    KeyCode::Char('m') => {
                        let z = self.spaces[self.active_space].zoom;
                        self.spaces[self.active_space].zoom = !z;
                    }
                    _ => {}
                }
            }
            Pending::LeaderT => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('n') => self.add_chat_to_active(),
                    KeyCode::Char('o') | KeyCode::Char('f') => {
                        self.new_space();
                        self.mode = Mode::Insert;
                    }
                    KeyCode::Char('x') => self.close_focused_pane(),
                    _ => {}
                }
            }
            Pending::LeaderE => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('e') => self.toggle_sidebar(),
                    KeyCode::Char('f') => self.focus_sidebar(),
                    KeyCode::Char('c') => {
                        self.sidebar_open = false;
                        self.focus = Focus::Main;
                    }
                    _ => {}
                }
            }
            Pending::LeaderN => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('c') => self.add_chat_to_active(),
                    KeyCode::Char('s') => {
                        self.new_space();
                    }
                    _ => {}
                }
            }
            Pending::LeaderZ => {
                self.pending = Pending::None;
                if let KeyCode::Char('z') = k.code {
                    self.help_open = true;
                }
            }
            Pending::None => {}
        }
    }

    // ---- focus / panes within the active space ----

    fn focus_sidebar(&mut self) {
        self.focus = Focus::Sidebar;
        self.sidebar_open = true;
        self.sidebar_cursor = self.active_space;
    }

    /// Space e e: toggle sidebar visibility (Space e f focuses it).
    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if !self.sidebar_open && self.focus == Focus::Sidebar {
            self.focus = Focus::Main;
        }
    }

    fn activate_selected(&mut self) {
        self.active_space = self.sidebar_cursor.min(self.spaces.len().saturating_sub(1));
        self.focus = Focus::Main;
    }

    fn focus_dir(&mut self, dir: Dir) {
        if self.focus == Focus::Sidebar {
            if let Dir::Right = dir {
                self.focus = Focus::Main;
            }
            return;
        }
        let sp = &self.spaces[self.active_space];
        let n = sp.chats.len();
        let cur = sp.fi();
        let target: Option<usize> = if n <= 1 {
            match dir {
                Dir::Left => Some(usize::MAX),
                _ => None,
            }
        } else if n == 2 {
            match (sp.split_dir, dir) {
                (SplitDir::V, Dir::Left) => Some(if cur == 1 { 0 } else { usize::MAX }),
                (SplitDir::V, Dir::Right) => (cur == 0).then_some(1),
                (SplitDir::H, Dir::Up) => (cur == 1).then_some(0),
                (SplitDir::H, Dir::Down) => (cur == 0).then_some(1),
                (SplitDir::H, Dir::Left) => Some(usize::MAX),
                _ => None,
            }
        } else if n == 3 {
            // TL=0, TR=1, bottom=2 (full width)
            match dir {
                Dir::Left => match cur {
                    1 => Some(0),
                    _ => Some(usize::MAX),
                },
                Dir::Right => (cur == 0).then_some(1),
                Dir::Down => (cur == 0 || cur == 1).then_some(2),
                Dir::Up => (cur == 2).then_some(0),
            }
        } else {
            // 2x2: TL=0 TR=1 BL=2 BR=3
            match dir {
                Dir::Left => match cur {
                    1 => Some(0),
                    3 => Some(2),
                    _ => Some(usize::MAX),
                },
                Dir::Right => match cur {
                    0 => Some(1),
                    2 => Some(3),
                    _ => None,
                },
                Dir::Up => match cur {
                    2 => Some(0),
                    3 => Some(1),
                    _ => None,
                },
                Dir::Down => match cur {
                    0 => Some(2),
                    1 => Some(3),
                    _ => None,
                },
            }
        };
        match target {
            Some(usize::MAX) => self.focus_sidebar(),
            Some(p) if p < n => self.spaces[self.active_space].focused = p,
            _ => {}
        }
    }

    fn pane_cycle(&mut self, d: isize) {
        let sp = &mut self.spaces[self.active_space];
        let n = sp.chats.len() as isize;
        if n < 2 {
            return;
        }
        sp.focused = (sp.fi() as isize + d).rem_euclid(n) as usize;
    }

    fn scroll_down(&mut self, n: u16) {
        let c = self.cur_chat_mut();
        if c.follow {
            c.scroll = c.last_max_scroll;
            c.follow = false;
        }
        c.scroll = c.scroll.saturating_add(n).min(c.last_max_scroll);
    }
    fn scroll_up(&mut self, n: u16) {
        let c = self.cur_chat_mut();
        if c.follow {
            c.scroll = c.last_max_scroll;
            c.follow = false;
        }
        c.scroll = c.scroll.saturating_sub(n);
    }

    fn sidebar_move(&mut self, delta: isize) {
        let n = self.spaces.len();
        if n == 0 {
            return;
        }
        let cur = self.sidebar_cursor.min(n - 1) as isize;
        self.sidebar_cursor = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    fn leader_jump(&mut self, d: char) {
        let idx = if d == '0' { 9 } else { (d as u8 - b'1') as usize };
        if idx < self.spaces.len() {
            self.active_space = idx;
            self.sidebar_cursor = idx;
            self.focus = Focus::Main;
        }
    }

    // ---- space / chat lifecycle ----

    fn next_chat_title(&mut self) -> String {
        let n = self.chat_counter;
        self.chat_counter += 1;
        format!("chat {n}")
    }

    fn next_space_name(&mut self) -> String {
        let n = self.space_counter;
        self.space_counter += 1;
        if n == 1 {
            "space".to_string()
        } else {
            format!("space {n}")
        }
    }

    fn new_space(&mut self) -> usize {
        let cid = self.next_chat_id;
        self.next_chat_id += 1;
        let sid = self.next_space_id;
        self.next_space_id += 1;
        let title = self.next_chat_title();
        let sname = self.next_space_name();
        let mut c = Chat::fresh(cid);
        c.title = title;
        let mut sp = Space::one(sid, c);
        sp.name = sname;
        self.spaces.push(sp);
        self.active_space = self.spaces.len() - 1;
        self.sidebar_cursor = self.active_space;
        self.persist();
        self.active_space
    }

    fn new_named_space(&mut self) {
        self.new_space();
        self.sidebar_open = true;
        self.focus = Focus::Sidebar;
        self.rename_target = RenameTarget::Space;
        self.rename_buf.clear();
        self.mode = Mode::Rename;
    }

    fn add_chat_to_active(&mut self) {
        if self.spaces[self.active_space].chats.len() >= 4 {
            return;
        }
        let cid = self.next_chat_id;
        self.next_chat_id += 1;
        let title = self.next_chat_title();
        {
            let sp = &mut self.spaces[self.active_space];
            let mut c = Chat::fresh(cid);
            c.title = title;
            sp.chats.push(c);
            sp.focused = sp.chats.len() - 1;
            sp.zoom = false;
        }
        self.focus = Focus::Main;
        self.persist();
    }

    fn pop_chat(&mut self) {
        let ai = self.active_space;
        if self.spaces[ai].chats.len() <= 1 {
            return;
        }
        let chat = {
            let sp = &mut self.spaces[ai];
            let f = sp.fi();
            let chat = sp.chats.remove(f);
            if sp.focused >= sp.chats.len() {
                sp.focused = sp.chats.len() - 1;
            }
            sp.zoom = false;
            chat
        };
        let sid = self.next_space_id;
        self.next_space_id += 1;
        let sname = self.next_space_name();
        let mut sp = Space::one(sid, chat);
        sp.name = sname;
        sp.cwd = self.spaces[ai].cwd.clone();
        self.spaces.push(sp);
        self.active_space = self.spaces.len() - 1;
        self.sidebar_cursor = self.active_space;
        self.focus = Focus::Main;
        self.persist();
    }

    fn close_focused_pane(&mut self) {
        let ai = self.active_space;
        if self.spaces[ai].chats.len() <= 1 {
            self.delete_space_at(ai);
        } else {
            let sp = &mut self.spaces[ai];
            let f = sp.fi();
            sp.chats.remove(f);
            if sp.focused >= sp.chats.len() {
                sp.focused = sp.chats.len() - 1;
            }
            sp.zoom = false;
        }
        self.persist();
    }

    fn delete_space_at(&mut self, i: usize) {
        if i >= self.spaces.len() {
            return;
        }
        self.spaces.remove(i);
        if self.spaces.is_empty() {
            self.active_space = 0;
            self.sidebar_cursor = 0;
            return; // empty — "start a space" state
        }
        if self.active_space >= self.spaces.len() {
            self.active_space = self.spaces.len() - 1;
        }
        if self.sidebar_cursor >= self.spaces.len() {
            self.sidebar_cursor = self.spaces.len() - 1;
        }
    }

    fn toggle_select(&mut self) {
        if let Some(id) = self.sel_space_id() {
            if let Some(pos) = self.selected.iter().position(|&x| x == id) {
                self.selected.remove(pos);
            } else {
                self.selected.push(id);
            }
        }
    }

    /// Merge the selected spaces into the first — chats combined (≤4), sources
    /// removed. Name defaults to the first space's name.
    fn merge_selected(&mut self) {
        if self.selected.len() < 2 {
            return;
        }
        let ids = self.selected.clone();
        let total: usize = ids
            .iter()
            .filter_map(|&id| self.space_index(id))
            .map(|i| self.spaces[i].chats.len())
            .sum();
        if total > 4 {
            self.cur_chat_mut()
                .transcript
                .push(Entry::Note("can't merge — would exceed 4 chats in a space".into()));
            self.selected.clear();
            return;
        }
        let target_name = self
            .space_index(ids[0])
            .map(|i| space_name(&self.spaces[i]))
            .unwrap_or_default();
        let mut moved: Vec<Chat> = Vec::new();
        for &oid in &ids[1..] {
            if let Some(oi) = self.space_index(oid) {
                let sp = self.spaces.remove(oi);
                moved.extend(sp.chats);
            }
        }
        if let Some(ti) = self.space_index(ids[0]) {
            self.spaces[ti].name = target_name;
            self.spaces[ti].chats.extend(moved);
            self.active_space = ti;
            self.sidebar_cursor = ti;
        }
        self.selected.clear();
        if self.active_space >= self.spaces.len() {
            self.active_space = self.spaces.len() - 1;
        }
        if self.sidebar_cursor >= self.spaces.len() {
            self.sidebar_cursor = self.spaces.len() - 1;
        }
        self.persist();
    }

    fn merge_space_into_active(&mut self, other_id: u64) {
        let ai = self.active_space;
        let a_id = self.spaces[ai].id;
        if other_id == a_id {
            return;
        }
        let Some(oi) = self.space_index(other_id) else {
            return;
        };
        if self.spaces[ai].chats.len() + self.spaces[oi].chats.len() > 4 {
            self.cur_chat_mut()
                .transcript
                .push(Entry::Note("can't merge — would exceed 4 chats in a space".into()));
            return;
        }
        let sp = self.spaces.remove(oi);
        let ai2 = self.space_index(a_id).unwrap_or(0);
        self.spaces[ai2].chats.extend(sp.chats);
        self.active_space = ai2;
        self.sidebar_cursor = ai2;
        self.persist();
    }

    fn request_delete(&mut self) {
        self.confirm_action = ConfirmAction::DeleteSpaces;
        let ids: Vec<u64> = if !self.selected.is_empty() {
            self.selected.clone()
        } else if let Some(id) = self.sel_space_id() {
            vec![id]
        } else {
            return;
        };
        let n = ids.len();
        self.confirm_msg = if n == 1 {
            let name = self
                .space_index(ids[0])
                .map(|i| space_name(&self.spaces[i]))
                .unwrap_or_default();
            format!("delete space \"{name}\"?   y / n")
        } else {
            format!("delete {n} spaces?   y / n")
        };
        self.pending_delete = ids;
        self.mode = Mode::Confirm;
    }

    fn rename_start(&mut self) {
        if self.focus == Focus::Main {
            // rename the focused chat (input shows in the composer)
            self.rename_target = RenameTarget::Chat;
            let ai = self.active_space;
            let fi = self.spaces[ai].fi();
            self.rename_buf = self.spaces[ai].chats[fi].title.clone();
        } else {
            // rename the space (inline in the sidebar)
            self.rename_target = RenameTarget::Space;
            let idx = self.sidebar_cursor.min(self.spaces.len().saturating_sub(1));
            self.sidebar_open = true;
            self.focus = Focus::Sidebar;
            self.sidebar_cursor = idx;
            self.rename_buf = self.spaces[idx].name.clone();
        }
        self.mode = Mode::Rename;
    }

    fn rename_commit(&mut self) {
        match self.rename_target {
            RenameTarget::Space => {
                let idx = self.sidebar_cursor.min(self.spaces.len().saturating_sub(1));
                self.spaces[idx].name = self.rename_buf.trim().to_string();
            }
            RenameTarget::Chat => {
                let ai = self.active_space;
                let fi = self.spaces[ai].fi();
                let name = self.rename_buf.trim().to_string();
                if !name.is_empty() {
                    self.spaces[ai].chats[fi].title = name;
                    self.spaces[ai].chats[fi].autonamed = true;
                }
            }
        }
        self.mode = Mode::Normal;
        self.persist();
    }

    // ---- picker (Space s c — merge a space in, or type a new chat name) ----

    fn open_picker(&mut self) {
        self.picker_query.clear();
        self.picker_sel = 0;
        self.mode = Mode::Picker;
    }

    pub fn picker_candidates(&self) -> Vec<usize> {
        let q = self.picker_query.to_lowercase();
        self.spaces
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.active_space)
            .filter(|(_, sp)| q.is_empty() || space_name(sp).to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn picker_down(&mut self) {
        let n = self.picker_candidates().len();
        if n > 0 {
            self.picker_sel = (self.picker_sel + 1).min(n - 1);
        }
    }
    fn picker_up(&mut self) {
        self.picker_sel = self.picker_sel.saturating_sub(1);
    }

    fn picker_commit(&mut self) {
        let cands = self.picker_candidates();
        if let Some(&si) = cands.get(self.picker_sel) {
            let other = self.spaces[si].id;
            self.merge_space_into_active(other);
        } else {
            // no match → add a new chat named the query to the active space
            let q = self.picker_query.trim().to_string();
            if !q.is_empty() && self.spaces[self.active_space].chats.len() < 4 {
                let cid = self.next_chat_id;
                self.next_chat_id += 1;
                let mut c = Chat::fresh(cid);
                c.title = slug(&q);
                c.autonamed = true;
                let sp = &mut self.spaces[self.active_space];
                sp.chats.push(c);
                sp.focused = sp.chats.len() - 1;
                self.persist();
            }
        }
        self.focus = Focus::Main;
        self.mode = Mode::Normal;
    }

    fn exec_command(&mut self) {
        let cmd = self.cmd.trim().to_string();
        self.cmd.clear();
        self.mode = Mode::Normal;
        let (head, rest) = match cmd.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (cmd.as_str(), ""),
        };
        match head {
            "q" | "quit" => {
                self.request_quit();
                return;
            }
            "q!" | "quit!" => {
                self.should_quit = true;
                return;
            }
            "new" => {
                self.new_space();
                self.mode = Mode::Insert;
                return;
            }
            "w" | "ws" | "write" if rest.is_empty() => {
                self.persist();
                self.flash = "state saved".into();
                return;
            }
            _ => {}
        }
        if self.spaces.is_empty() {
            return;
        }
        match head {
            // :w <file> — export the focused chat's transcript as markdown
            "w" | "write" => self.export_transcript(rest),
            "close" => self.close_focused_pane(),
            "pop" => self.pop_chat(),
            "vsplit" | "vs" => self.spaces[self.active_space].split_dir = SplitDir::V,
            "split" | "sp" => self.spaces[self.active_space].split_dir = SplitDir::H,
            "cd" => self.set_space_cwd(rest),
            "all" => self.broadcast(rest),
            "model" => self.set_chat_model(rest),
            _ => self.flash = format!("unknown command: :{head}"),
        }
    }

    fn export_transcript(&mut self, path: &str) {
        if path.is_empty() {
            self.flash = "usage: :w <file>".into();
            return;
        }
        let path = expand_home(path);
        let c = self.cur_chat();
        let mut out = format!("# {}\n\n", chat_title(c));
        for e in &c.transcript {
            match e {
                Entry::User(t) => out.push_str(&format!("## you\n\n{t}\n\n")),
                Entry::Assistant(t) => out.push_str(&format!("## claude\n\n{t}\n\n")),
                Entry::Tool(t) => {
                    out.push_str(&format!("> {}\n\n", t.replace('\n', "\n> ")))
                }
                Entry::ToolResult { ok, text } => {
                    let first = tool_result_summary(text);
                    let tag = if *ok { "" } else { "ERROR: " };
                    out.push_str(&format!("> ⎿ {tag}{first}\n\n"));
                }
                Entry::Note(t) => out.push_str(&format!("_{t}_\n\n")),
                Entry::Error(t) => out.push_str(&format!("**error:** {t}\n\n")),
            }
        }
        match std::fs::write(&path, out) {
            Ok(()) => self.flash = format!("wrote {path}"),
            Err(e) => self.flash = format!("write failed: {e}"),
        }
    }

    /// :cd <dir> — set the active space's working directory (empty resets).
    fn set_space_cwd(&mut self, path: &str) {
        if path.is_empty() {
            self.spaces[self.active_space].cwd = None;
            self.flash = "cwd reset to launch dir".into();
            self.persist();
            return;
        }
        let expanded = expand_home(path);
        match std::fs::canonicalize(&expanded) {
            Ok(p) if p.is_dir() => {
                let s = p.display().to_string();
                self.spaces[self.active_space].cwd = Some(s.clone());
                self.flash = format!("cwd: {s}");
                self.persist();
            }
            _ => self.flash = format!("not a directory: {expanded}"),
        }
    }

    /// :all <prompt> — send one prompt to every chat in the active space.
    fn broadcast(&mut self, prompt: &str) {
        if prompt.is_empty() {
            self.flash = "usage: :all <prompt>".into();
            return;
        }
        let ai = self.active_space;
        let n = self.spaces[ai].chats.len();
        for ci in 0..n {
            if self.spaces[ai].chats[ci].in_flight {
                self.spaces[ai].chats[ci].queued.push(prompt.to_string());
            } else {
                self.spawn_for(ai, ci, prompt.to_string());
            }
        }
        self.flash = format!("sent to {n} chat{}", if n == 1 { "" } else { "s" });
    }

    /// :model <name> — per-chat model override (empty resets to default).
    fn set_chat_model(&mut self, name: &str) {
        let ai = self.active_space;
        let fi = self.spaces[ai].fi();
        if name.is_empty() {
            self.spaces[ai].chats[fi].model = None;
            self.flash = format!("model: default ({})", self.model_display);
        } else {
            self.spaces[ai].chats[fi].model = Some(name.to_string());
            self.flash = format!("model: {name} (this chat)");
        }
        self.persist();
    }

    fn env_prompt_for(&self, si: usize, ci: usize) -> String {
        let cwd = self.spaces[si].cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
        let me = chat_title(&self.spaces[si].chats[ci]);
        let my_space = space_name(&self.spaces[si]);
        let others: Vec<String> = self
            .spaces
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != si)
            .map(|(_, sp)| space_name(sp))
            .collect();
        let pipe = std::env::var("AEOVIM_PIPE").ok();
        let pipe_instr = match &pipe {
            Some(p) if !others.is_empty() => format!(
                " You can message another space's agent by appending ONE JSON line to the pipe at {p}, e.g.  echo '{{\"to\":\"<space name>\",\"from\":\"{my_space}\",\"message\":\"...\"}}' >> {p}  — only when you genuinely need to coordinate with another agent. Spaces you can message: [{}].",
                others.join(", ")
            ),
            _ => String::new(),
        };
        format!(
            "You are running inside aeovim — a keyboard-driven, multi-agent terminal UI that wraps the Claude Code CLI. \
You are the agent \"{me}\" in the space \"{my_space}\". The user may run several agents in parallel.{pipe_instr} \
Working directory: {cwd}. You're in a terminal on macOS (tmux/Ghostty) — keep output concise and terminal-friendly."
        )
    }

    /// Push a user prompt to a specific chat and spawn its turn.
    fn spawn_for(&mut self, si: usize, ci: usize, prompt: String) {
        if si >= self.spaces.len() || ci >= self.spaces[si].chats.len() {
            return;
        }
        if self.spaces[si].chats[ci].in_flight {
            return;
        }
        let dangerous = self.dangerous;
        let model = self.spaces[si].chats[ci]
            .model
            .clone()
            .or_else(|| self.model_cli.clone());
        let tx = self.tx.clone();
        let sysp = self.env_prompt_for(si, ci);
        let sname = space_name(&self.spaces[si]);
        let cwd = self.spaces[si].cwd.clone();
        let pipe = std::env::var("AEOVIM_PIPE").ok();
        let spec = {
            let c = &mut self.spaces[si].chats[ci];
            if c.first_turn && !c.autonamed {
                c.title = slug(&prompt);
            }
            c.transcript.push(Entry::User(prompt.clone()));
            c.streaming = None;
            c.thinking = None;
            c.attention = None;
            c.in_flight = true;
            c.follow = true;
            c.last_prompt = prompt.clone();
            let spec = TurnSpec {
                chat: c.id,
                prompt,
                session_id: c.session_id.clone(),
                first: c.first_turn,
                model,
                dangerous,
                permission_mode: "acceptEdits".into(),
                append_system_prompt: Some(sysp),
                space_name: sname,
                pipe_path: pipe,
                cwd,
            };
            c.first_turn = false;
            spec
        };
        spawn_turn(spec, tx);
        self.persist();
    }

    fn send_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() || self.spaces.is_empty() {
            return;
        }
        let ai = self.active_space;
        let fi = self.spaces[ai].fi();
        // /clear: wipe the transcript and start a fresh claude session
        if prompt == "/clear" {
            let c = &mut self.spaces[ai].chats[fi];
            c.transcript.clear();
            c.transcript.push(Entry::Note("cleared".into()));
            c.streaming = None;
            c.thinking = None;
            c.cost = 0.0;
            c.context_tokens = 0;
            c.queued.clear();
            c.attention = None;
            c.session_retry = false;
            c.first_turn = true;
            c.session_id = Uuid::new_v4().to_string();
            self.input.clear();
            self.input_cursor = 0;
            self.mode = Mode::Normal;
            self.persist();
            return;
        }
        self.input.clear();
        self.input_cursor = 0;
        self.mode = Mode::Normal;
        if self.spaces[ai].chats[fi].in_flight {
            // type-ahead: queue and auto-send when the current turn finishes
            let c = &mut self.spaces[ai].chats[fi];
            c.queued.push(prompt);
            c.transcript.push(Entry::Note(format!(
                "queued — will send when this turn finishes ({} waiting)",
                c.queued.len()
            )));
            return;
        }
        self.spawn_for(ai, fi, prompt);
    }

    /// A message arrived over the pipe from another agent — deliver it to the
    /// named space's focused chat and let that agent respond (shown in the UI).
    fn inject_pipe(&mut self, to: String, from: String, message: String) {
        if self.spaces.is_empty() {
            return;
        }
        let target = self
            .spaces
            .iter()
            .position(|sp| space_name(sp).eq_ignore_ascii_case(to.trim()));
        let Some(si) = target else {
            self.cur_chat_mut()
                .transcript
                .push(Entry::Note(format!("pipe: no space named \"{to}\"")));
            return;
        };
        let ci = self.spaces[si].fi();
        let who = if from.trim().is_empty() {
            "another agent".to_string()
        } else {
            from.trim().to_string()
        };
        let prompt = format!("[message from space \"{who}\" via aeovim pipe]\n{message}");
        if self.spaces[si].chats[ci].in_flight {
            // deliver when the current turn finishes (drained in turn_ended)
            let c = &mut self.spaces[si].chats[ci];
            c.queued.push(prompt);
            c.transcript.push(Entry::Note(format!(
                "pipe from {who} queued — delivering when this turn finishes"
            )));
            return;
        }
        self.spawn_for(si, ci, prompt);
    }
}
