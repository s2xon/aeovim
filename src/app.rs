//! App state + update logic. Keymap ported from the user's Neovim config:
//! leader = Space (which-key tree), Ctrl-hjkl pane focus, nvim-tree sidebar keys
//! (a add, r rename, d delete), harpoon-style Space+number jump, bufferline H/L.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::agent::{spawn_turn, TurnSpec};
use crate::protocol::AgentEvent;
use crate::store::{self, PersistChat};

#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Rename,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus {
    Sidebar,
    Main,
}

/// Multi-key prefix state (mirrors which-key leader chords + `g`).
#[derive(PartialEq, Clone, Copy)]
pub enum Pending {
    None,
    G,
    Leader,
    LeaderE,
    LeaderS,
    LeaderT,
    LeaderZ,
}

pub enum Entry {
    User(String),
    Assistant(String),
    Tool(String),
    Note(String),
    Error(String),
}

pub struct Chat {
    pub id: u64,
    pub title: String,
    pub qualifier: Option<String>,
    pub autonamed: bool,
    pub transcript: Vec<Entry>,
    pub streaming: Option<String>,
    pub in_flight: bool,
    pub session_id: String,
    pub first_turn: bool,
    pub cost: f64,
    pub scroll: u16,
    pub follow: bool,
    pub last_max_scroll: u16,
}

impl Chat {
    fn fresh(id: u64) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let mut transcript = Vec::new();
        transcript.push(Entry::Note(format!("session {}", &session_id[..8])));
        Chat {
            id,
            title: String::new(), // unnamed until named or auto-named after first turn
            qualifier: None,
            autonamed: false,
            transcript,
            streaming: None,
            in_flight: false,
            session_id,
            first_turn: true,
            cost: 0.0,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
        }
    }

    fn from_persist(id: u64, pc: PersistChat) -> Self {
        let mut transcript = Vec::new();
        transcript.push(Entry::Note(format!(
            "resumed · session {} (context preserved — send a message to continue)",
            &pc.session_id[..8.min(pc.session_id.len())]
        )));
        Chat {
            id,
            title: pc.title,
            qualifier: pc.qualifier,
            autonamed: true,
            transcript,
            streaming: None,
            in_flight: false,
            session_id: pc.session_id,
            first_turn: false,
            cost: pc.cost,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
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

pub enum Msg {
    Input(Event),
    Tick,
    Agent { chat: u64, ev: AgentEvent },
    TurnEnded { chat: u64, error: Option<String> },
}

pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub pending: Pending,
    pub input: String,
    pub cmd: String,
    pub rename_buf: String,
    pub chats: Vec<Chat>,
    pub active: usize,
    pub sidebar_cursor: usize,
    pub sidebar_open: bool,
    pub model_cli: Option<String>,
    pub model_display: String,
    pub dangerous: bool,
    pub should_quit: bool,
    pub spinner: usize,
    pub help_open: bool,
    next_id: u64,
    workspace_key: String,
    tx: UnboundedSender<Msg>,
}

impl App {
    pub fn new(
        model_cli: Option<String>,
        dangerous: bool,
        tx: UnboundedSender<Msg>,
        workspace_key: String,
        restored: Vec<PersistChat>,
    ) -> Self {
        let model_display = model_cli.clone().unwrap_or_else(|| "default".into());
        let mut chats = Vec::new();
        let mut next_id = 1u64;
        if restored.is_empty() {
            chats.push(Chat::fresh(next_id));
            next_id += 1;
        } else {
            for pc in restored {
                chats.push(Chat::from_persist(next_id, pc));
                next_id += 1;
            }
        }
        Self {
            mode: Mode::Normal,
            focus: Focus::Main,
            pending: Pending::None,
            input: String::new(),
            cmd: String::new(),
            rename_buf: String::new(),
            chats,
            active: 0,
            sidebar_cursor: 0,
            sidebar_open: true,
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            help_open: false,
            next_id,
            workspace_key,
            tx,
        }
    }

    pub fn active_chat(&self) -> &Chat {
        &self.chats[self.active]
    }
    fn active_chat_mut(&mut self) -> &mut Chat {
        &mut self.chats[self.active]
    }
    fn chat_by_id(&mut self, id: u64) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.id == id)
    }

    pub fn persist(&self) {
        let data: Vec<PersistChat> = self
            .chats
            .iter()
            .map(|c| PersistChat {
                title: c.title.clone(),
                qualifier: c.qualifier.clone(),
                session_id: c.session_id.clone(),
                cost: c.cost,
            })
            .collect();
        store::save(&self.workspace_key, &data);
    }

    /// Display order: flat (unqualified) first, then each qualifier group.
    pub fn visible_order(&self) -> Vec<usize> {
        let mut order = Vec::new();
        for (i, c) in self.chats.iter().enumerate() {
            if c.qualifier.is_none() {
                order.push(i);
            }
        }
        let mut quals: Vec<String> = Vec::new();
        for c in &self.chats {
            if let Some(q) = &c.qualifier {
                if !quals.contains(q) {
                    quals.push(q.clone());
                }
            }
        }
        for q in &quals {
            for (i, c) in self.chats.iter().enumerate() {
                if c.qualifier.as_ref() == Some(q) {
                    order.push(i);
                }
            }
        }
        order
    }

    /// Indices in the same qualifier group as the active chat (for Space+digit).
    fn category_indices(&self) -> Vec<usize> {
        let q = self.chats[self.active].qualifier.clone();
        self.visible_order()
            .into_iter()
            .filter(|&i| self.chats[i].qualifier == q)
            .collect()
    }

    fn sync_cursor(&mut self) {
        let order = self.visible_order();
        self.sidebar_cursor = order.iter().position(|&i| i == self.active).unwrap_or(0);
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => self.spinner = self.spinner.wrapping_add(1),
            Msg::Input(Event::Key(k)) => self.handle_key(k),
            Msg::Input(_) => {}
            Msg::Agent { chat, ev } => self.handle_agent(chat, ev),
            Msg::TurnEnded { chat, error } => {
                if let Some(c) = self.chat_by_id(chat) {
                    if c.in_flight {
                        c.commit_streaming();
                        c.in_flight = false;
                        c.follow = true;
                    }
                    if let Some(e) = error {
                        c.transcript.push(Entry::Error(e));
                    }
                }
                self.persist();
            }
        }
    }

    fn handle_agent(&mut self, id: u64, ev: AgentEvent) {
        let is_result = matches!(ev, AgentEvent::TurnResult { .. });
        let mut model_update = None;
        if let Some(c) = self.chat_by_id(id) {
            match ev {
                AgentEvent::Init { session_id, model } => {
                    if let Some(s) = session_id {
                        c.session_id = s;
                    }
                    if let Some(m) = model {
                        model_update = Some(m);
                    }
                }
                AgentEvent::TextDelta(s) => {
                    c.follow = true;
                    c.streaming.get_or_insert_with(String::new).push_str(&s);
                }
                AgentEvent::AssistantFinal(s) => {
                    if c.streaming.as_ref().map_or(true, |x| x.trim().is_empty()) {
                        c.streaming = Some(s);
                    }
                }
                AgentEvent::ToolUse(name) => {
                    c.commit_streaming();
                    c.transcript.push(Entry::Tool(name));
                    c.follow = true;
                }
                AgentEvent::TurnResult { cost_usd, is_error, text } => {
                    if c.streaming.as_ref().map_or(true, |x| x.trim().is_empty()) {
                        if let Some(t) = text {
                            if !t.trim().is_empty() {
                                c.streaming = Some(t);
                            }
                        }
                    }
                    c.commit_streaming();
                    c.cost += cost_usd;
                    c.in_flight = false;
                    if is_error {
                        c.transcript.push(Entry::Error("turn ended with error".into()));
                    }
                    c.follow = true;
                    // Auto-name from the first user prompt after the first turn.
                    if !c.autonamed {
                        if let Some(p) = c.transcript.iter().find_map(|e| match e {
                            Entry::User(t) => Some(t.clone()),
                            _ => None,
                        }) {
                            c.title = slug(&p);
                            c.autonamed = true;
                        }
                    }
                }
                AgentEvent::Ignore => {}
            }
        }
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(m) = model_update {
            self.model_display = m;
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
        if self.help_open {
            self.help_open = false; // any key dismisses the cheatsheet
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Command => match k.code {
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
            },
            Mode::Rename => match k.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => self.rename_commit(),
                KeyCode::Backspace => {
                    self.rename_buf.pop();
                }
                KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
                KeyCode::Char('u') if ctrl => self.rename_buf.clear(),
                KeyCode::Char(c) => self.rename_buf.push(c),
                _ => {}
            },
            Mode::Insert => match k.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => self.send_prompt(),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char('u') if ctrl => self.input.clear(),
                KeyCode::Char('c') if ctrl => self.should_quit = true,
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            },
            Mode::Normal => {
                if self.pending != Pending::None {
                    self.handle_pending(k);
                    return;
                }
                match k.code {
                    KeyCode::Char('c') if ctrl => self.should_quit = true,
                    KeyCode::Char('h') if ctrl => self.focus_sidebar(),
                    KeyCode::Char('l') if ctrl => self.focus = Focus::Main,
                    KeyCode::Char('e') if ctrl => self.focus_sidebar(), // harpoon quick menu
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
                        self.active_chat_mut().follow = true;
                    }
                    KeyCode::Char('q') => self.should_quit = true,
                    KeyCode::Char('H') => self.cycle(-1),
                    KeyCode::Char('L') => self.cycle(1),
                    KeyCode::Char('g') => self.pending = Pending::G,
                    KeyCode::Char('G') => self.active_chat_mut().follow = true,
                    KeyCode::Char('n') => {
                        self.new_chat();
                        self.mode = Mode::Insert;
                    }
                    // nvim-tree keys (only when the sidebar is focused)
                    KeyCode::Char('a') if self.focus == Focus::Sidebar => self.new_named_chat(),
                    KeyCode::Char('r') if self.focus == Focus::Sidebar => self.rename_start(),
                    KeyCode::Char('d') if self.focus == Focus::Sidebar => self.delete_active(),
                    KeyCode::Char('x') => self.delete_active(),
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
                    KeyCode::Char('h') | KeyCode::Left => {
                        if self.focus == Focus::Main {
                            self.focus_sidebar()
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                        if self.focus == Focus::Sidebar {
                            self.focus = Focus::Main
                        }
                    }
                    KeyCode::Tab => self.cycle(1),
                    KeyCode::BackTab => self.cycle(-1),
                    _ => {}
                }
            }
        }
    }

    fn handle_pending(&mut self, k: KeyEvent) {
        match self.pending {
            Pending::G => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('g') => {
                        let c = self.active_chat_mut();
                        c.follow = false;
                        c.scroll = 0;
                    }
                    KeyCode::Char('t') => self.cycle(1),
                    KeyCode::Char('T') => self.cycle(-1),
                    _ => {}
                }
            }
            Pending::Leader => match k.code {
                KeyCode::Char('b') => {
                    self.pending = Pending::None;
                    self.toggle_sidebar();
                }
                KeyCode::Char('z') => self.pending = Pending::LeaderZ,
                KeyCode::Char('e') => self.pending = Pending::LeaderE,
                KeyCode::Char('s') => self.pending = Pending::LeaderS,
                KeyCode::Char('t') => self.pending = Pending::LeaderT,
                KeyCode::Char('a') => {
                    self.pending = Pending::None;
                    self.new_named_chat();
                }
                KeyCode::Char(d @ '0'..='9') => {
                    self.pending = Pending::None;
                    self.leader_jump(d);
                }
                _ => self.pending = Pending::None,
            },
            Pending::LeaderE => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('e') => {
                        self.sidebar_open = !self.sidebar_open;
                        if !self.sidebar_open {
                            self.focus = Focus::Main;
                        }
                    }
                    KeyCode::Char('f') => self.focus_sidebar(),
                    KeyCode::Char('c') => {
                        self.sidebar_open = false;
                        self.focus = Focus::Main;
                    }
                    KeyCode::Char('r') => self.persist(),
                    _ => {}
                }
            }
            Pending::LeaderS => {
                self.pending = Pending::None;
                self.active_chat_mut().transcript.push(Entry::Note(
                    "splits (1 · 1×2 · 2×2) + zoom — coming next".into(),
                ));
            }
            Pending::LeaderT => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('o') | KeyCode::Char('f') => self.new_chat(),
                    KeyCode::Char('x') => self.delete_active(),
                    KeyCode::Char('n') => self.cycle(1),
                    KeyCode::Char('p') => self.cycle(-1),
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

    fn focus_sidebar(&mut self) {
        self.focus = Focus::Sidebar;
        self.sidebar_open = true;
        self.sync_cursor();
    }

    fn sidebar_move(&mut self, delta: isize) {
        let order = self.visible_order();
        if order.is_empty() {
            return;
        }
        let cur = self.sidebar_cursor.min(order.len() - 1) as isize;
        let next = (cur + delta).clamp(0, order.len() as isize - 1) as usize;
        self.sidebar_cursor = next;
        self.active = order[next];
    }

    fn leader_jump(&mut self, d: char) {
        let idx = (d as u8 - b'0') as usize;
        let cat = self.category_indices();
        if idx < cat.len() {
            self.active = cat[idx];
            self.sync_cursor();
        }
    }

    fn cycle(&mut self, d: isize) {
        let order = self.visible_order();
        if order.len() < 2 {
            return;
        }
        let pos = order.iter().position(|&i| i == self.active).unwrap_or(0) as isize;
        let n = (pos + d).rem_euclid(order.len() as isize) as usize;
        self.active = order[n];
        self.sidebar_cursor = n;
    }

    fn scroll_down(&mut self, n: u16) {
        let c = self.active_chat_mut();
        if c.follow {
            c.scroll = c.last_max_scroll;
            c.follow = false;
        }
        c.scroll = c.scroll.saturating_add(n).min(c.last_max_scroll);
    }
    fn scroll_up(&mut self, n: u16) {
        let c = self.active_chat_mut();
        if c.follow {
            c.scroll = c.last_max_scroll;
            c.follow = false;
        }
        c.scroll = c.scroll.saturating_sub(n);
    }

    fn create_chat(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.chats.push(Chat::fresh(id));
        self.active = self.chats.len() - 1;
        self.sync_cursor();
        self.persist();
    }

    fn new_chat(&mut self) {
        self.create_chat();
        self.focus = Focus::Main;
    }

    /// Add a chat and name it in the sidebar: focus stays on the sidebar, the
    /// new (unnamed) entry is highlighted and prompts inline for a name.
    fn new_named_chat(&mut self) {
        self.create_chat();
        self.sidebar_open = true;
        self.focus = Focus::Sidebar;
        self.rename_buf.clear();
        self.mode = Mode::Rename;
    }

    /// Space b: toggle the sidebar and move focus with it.
    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.focus = Focus::Sidebar;
            self.sync_cursor();
        } else {
            self.focus = Focus::Main;
        }
    }

    fn rename_start(&mut self) {
        self.sidebar_open = true;
        self.focus = Focus::Sidebar;
        self.rename_buf = self.chats[self.active].title.clone();
        self.mode = Mode::Rename;
    }

    fn rename_commit(&mut self) {
        let name = self.rename_buf.trim().to_string();
        if name.is_empty() {
            // No name: use the first prompt if any, else leave it unnamed so it
            // auto-names after the first turn.
            let auto = self.chats[self.active]
                .transcript
                .iter()
                .find_map(|e| match e {
                    Entry::User(t) => Some(slug(t)),
                    _ => None,
                });
            if let Some(a) = auto {
                self.chats[self.active].title = a;
                self.chats[self.active].autonamed = true;
            }
        } else {
            self.chats[self.active].title = name;
            self.chats[self.active].autonamed = true;
        }
        self.mode = Mode::Normal;
        self.persist();
    }

    fn delete_active(&mut self) {
        if self.chats.len() <= 1 {
            return;
        }
        self.chats.remove(self.active);
        if self.active >= self.chats.len() {
            self.active = self.chats.len() - 1;
        }
        self.sync_cursor();
        self.persist();
    }

    fn set_qualifier(&mut self, q: Option<String>) {
        self.chats[self.active].qualifier = q;
        self.sync_cursor();
        self.persist();
    }

    fn exec_command(&mut self) {
        let cmd = self.cmd.trim().to_string();
        self.cmd.clear();
        self.mode = Mode::Normal;
        match cmd.as_str() {
            "q" | "quit" => self.should_quit = true,
            "new" => self.new_chat(),
            "loop" => self.set_qualifier(Some("loop".into())),
            "agent" => self.set_qualifier(Some("agent".into())),
            "parallel" => self.set_qualifier(Some("parallel".into())),
            "solo" | "clear" | "unqualify" => self.set_qualifier(None),
            "close" => self.delete_active(),
            "w" | "ws" | "write" => self.persist(),
            _ => {}
        }
    }

    fn env_prompt(&self, idx: usize) -> String {
        let chat = &self.chats[idx];
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let others: Vec<&str> = self
            .chats
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, c)| c.title.as_str())
            .collect();
        let role = chat
            .qualifier
            .as_ref()
            .map(|q| format!(", acting as a \"{q}\" agent"))
            .unwrap_or_default();
        format!(
            "You are running inside aeovim — a keyboard-driven, multi-agent terminal UI that wraps the Claude Code CLI. \
You are the agent labelled \"{}\"{}. The user may run several agents in parallel; sibling agents currently open: [{}]. \
Agents persist across restarts and (soon) can trigger one another. Working directory: {}. \
You're in a terminal on macOS (tmux/Ghostty) — keep output concise and terminal-friendly.",
            chat.title,
            role,
            others.join(", "),
            cwd
        )
    }

    fn send_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // `/loop` is an aeovim control for now: tag the chat, don't spawn a turn.
        if prompt.starts_with("/loop") {
            self.set_qualifier(Some("loop".into()));
            self.active_chat_mut()
                .transcript
                .push(Entry::Note("marked as loop — scheduler coming soon".into()));
            self.input.clear();
            self.mode = Mode::Normal;
            return;
        }

        let dangerous = self.dangerous;
        let model = self.model_cli.clone();
        let tx = self.tx.clone();
        let sysp = self.env_prompt(self.active);

        let spec = {
            let c = &mut self.chats[self.active];
            if c.in_flight {
                return;
            }
            c.transcript.push(Entry::User(prompt.clone()));
            c.streaming = None;
            c.in_flight = true;
            c.follow = true;
            let spec = TurnSpec {
                chat: c.id,
                prompt,
                session_id: c.session_id.clone(),
                first: c.first_turn,
                model,
                dangerous,
                permission_mode: "acceptEdits".into(),
                append_system_prompt: Some(sysp),
            };
            c.first_turn = false;
            spec
        };

        self.input.clear();
        self.mode = Mode::Normal;
        spawn_turn(spec, tx);
    }
}
