//! App state + update logic for the aeovim shell.
//!
//! Multi-chat from the start: `chats: Vec<Chat>`, one focused (`active`), a
//! toggleable sidebar that groups chats into Chats / Parallel / Looping /
//! Previous. Keybinds here are PROVISIONAL — the real, nvim-mirroring keymap
//! comes later. This file is the shell's spine.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::agent::{spawn_turn, TurnSpec};
use crate::protocol::AgentEvent;

#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ChatKind {
    Solo,
    Parallel,
    Loop,
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
    pub kind: ChatKind,
    pub transcript: Vec<Entry>,
    pub streaming: Option<String>,
    pub in_flight: bool,
    pub session_id: String,
    pub first_turn: bool,
    pub cost: f64,
    pub scroll: u16,
    pub follow: bool,
    pub last_max_scroll: u16,
    pub closed: bool,
}

impl Chat {
    fn new(id: u64, title: String, kind: ChatKind) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let mut transcript = Vec::new();
        transcript.push(Entry::Note(format!(
            "{} · session {}",
            kind_label(kind),
            &session_id[..8]
        )));
        Chat {
            id,
            title,
            kind,
            transcript,
            streaming: None,
            in_flight: false,
            session_id,
            first_turn: true,
            cost: 0.0,
            scroll: 0,
            follow: true,
            last_max_scroll: 0,
            closed: false,
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

fn kind_label(k: ChatKind) -> &'static str {
    match k {
        ChatKind::Solo => "solo agent",
        ChatKind::Parallel => "parallel agent",
        ChatKind::Loop => "loop agent",
    }
}

pub enum Msg {
    Input(Event),
    Tick,
    Agent { chat: u64, ev: AgentEvent },
    TurnEnded { chat: u64, error: Option<String> },
}

pub struct App {
    pub mode: Mode,
    pub input: String,
    pub chats: Vec<Chat>,
    pub active: usize,
    pub sidebar_open: bool,
    pub model_cli: Option<String>,
    pub model_display: String,
    pub dangerous: bool,
    pub should_quit: bool,
    pub spinner: usize,
    pub pending_g: bool,
    next_id: u64,
    tx: UnboundedSender<Msg>,
}

impl App {
    pub fn new(model_cli: Option<String>, dangerous: bool, tx: UnboundedSender<Msg>) -> Self {
        let model_display = model_cli.clone().unwrap_or_else(|| "default".into());
        let first = Chat::new(1, "chat 1".into(), ChatKind::Solo);
        Self {
            mode: Mode::Normal,
            input: String::new(),
            chats: vec![first],
            active: 0,
            sidebar_open: true,
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            pending_g: false,
            next_id: 2,
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
            }
        }
    }

    fn handle_agent(&mut self, id: u64, ev: AgentEvent) {
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
                }
                AgentEvent::Ignore => {}
            }
        }
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(m) = model_update {
            self.model_display = m;
        }
    }

    // ---- input (provisional keymap) ----

    fn handle_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
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
                if self.pending_g {
                    self.pending_g = false;
                    match k.code {
                        KeyCode::Char('g') => {
                            let c = self.active_chat_mut();
                            c.follow = false;
                            c.scroll = 0;
                        }
                        KeyCode::Char('t') => self.next_chat(),
                        KeyCode::Char('T') => self.prev_chat(),
                        _ => {}
                    }
                    return;
                }
                match k.code {
                    KeyCode::Char('c') if ctrl => self.should_quit = true,
                    KeyCode::Char('d') if ctrl => self.scroll_down(8),
                    KeyCode::Char('u') if ctrl => self.scroll_up(8),
                    KeyCode::Char('i') => {
                        self.mode = Mode::Insert;
                        self.active_chat_mut().follow = true;
                    }
                    KeyCode::Char('q') => self.should_quit = true,
                    KeyCode::Char('b') => self.sidebar_open = !self.sidebar_open,
                    KeyCode::Char('n') => self.new_chat(ChatKind::Solo),
                    KeyCode::Char('p') => self.new_chat(ChatKind::Parallel),
                    KeyCode::Char('l') => self.new_chat(ChatKind::Loop),
                    KeyCode::Char('x') => self.close_active(),
                    KeyCode::Char('g') => self.pending_g = true,
                    KeyCode::Char('G') => self.active_chat_mut().follow = true,
                    KeyCode::Char('j') | KeyCode::Down => self.scroll_down(1),
                    KeyCode::Char('k') | KeyCode::Up => self.scroll_up(1),
                    KeyCode::Tab => self.next_chat(),
                    KeyCode::BackTab => self.prev_chat(),
                    _ => {}
                }
            }
        }
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

    fn open_indices(&self) -> Vec<usize> {
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.closed)
            .map(|(i, _)| i)
            .collect()
    }
    fn next_chat(&mut self) {
        let open = self.open_indices();
        if open.len() < 2 {
            return;
        }
        let pos = open.iter().position(|&i| i == self.active).unwrap_or(0);
        self.active = open[(pos + 1) % open.len()];
    }
    fn prev_chat(&mut self) {
        let open = self.open_indices();
        if open.len() < 2 {
            return;
        }
        let pos = open.iter().position(|&i| i == self.active).unwrap_or(0);
        self.active = open[(pos + open.len() - 1) % open.len()];
    }

    fn new_chat(&mut self, kind: ChatKind) {
        let id = self.next_id;
        self.next_id += 1;
        let n = self.chats.len() + 1;
        self.chats.push(Chat::new(id, format!("chat {n}"), kind));
        self.active = self.chats.len() - 1;
        self.mode = Mode::Insert;
    }

    fn close_active(&mut self) {
        if self.open_indices().len() <= 1 {
            return; // always keep one live chat
        }
        self.chats[self.active].closed = true;
        if let Some(&i) = self.open_indices().first() {
            self.active = i;
        }
    }

    fn send_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let dangerous = self.dangerous;
        let model = self.model_cli.clone();
        let tx = self.tx.clone();

        let spec = {
            let c = self.active_chat_mut();
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
            };
            c.first_turn = false;
            spec
        };

        self.input.clear();
        self.mode = Mode::Normal;
        spawn_turn(spec, tx);
    }
}
