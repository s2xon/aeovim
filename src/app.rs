//! App state + update logic. Keymap ported from the user's nvim (leader = Space,
//! Ctrl-hjkl focus, nvim-tree sidebar keys, harpoon number-jump).
//!
//! Panes: the main area holds one or more *panes* (slots) arranged as a vertical
//! split, horizontal split, or 2x2 grid. Each pane holds an ordered list of
//! chats as *inter-tabs*; `Tab` cycles tabs within the focused pane only.
//! `Space s c` fuzzy-finds a chat and merges it into the focused pane.

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
    Picker,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(PartialEq, Clone, Copy)]
pub enum SplitDir {
    V,
    H,
}

/// What the fuzzy-picker does on commit.
#[derive(PartialEq, Clone, Copy)]
enum PickerAction {
    AddTab,   // Space s c — add the chosen chat as a tab in this space
    OpenHere, // Space s v/h — fill the freshly-split space with the chosen chat
}

#[derive(PartialEq, Clone, Copy)]
pub enum Pending {
    None,
    G,
    Leader,
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
            title: String::new(),
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

/// A slot on screen holding one or more chats as inter-tabs.
pub struct Pane {
    pub tabs: Vec<u64>, // chat ids
    pub active_tab: usize,
}

impl Pane {
    fn current(&self) -> u64 {
        self.tabs[self.active_tab.min(self.tabs.len().saturating_sub(1))]
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
    pub picker_query: String,
    pub picker_sel: usize,
    picker_action: PickerAction,
    pub chats: Vec<Chat>,
    pub panes: Vec<Pane>,
    pub active_pane: usize,
    pub split_dir: SplitDir,
    pub zoom: bool,
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
        let first_id = chats[0].id;
        Self {
            mode: Mode::Normal,
            focus: Focus::Main,
            pending: Pending::None,
            input: String::new(),
            cmd: String::new(),
            rename_buf: String::new(),
            picker_query: String::new(),
            picker_sel: 0,
            picker_action: PickerAction::AddTab,
            chats,
            panes: vec![Pane {
                tabs: vec![first_id],
                active_tab: 0,
            }],
            active_pane: 0,
            split_dir: SplitDir::V,
            zoom: false,
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

    // ---- chat / pane lookups ----

    fn chat_index(&self, id: u64) -> Option<usize> {
        self.chats.iter().position(|c| c.id == id)
    }
    fn chat_by_id(&mut self, id: u64) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.id == id)
    }
    pub fn cur_id(&self) -> u64 {
        self.panes[self.active_pane].current()
    }
    fn cur_idx(&self) -> usize {
        self.chat_index(self.cur_id()).unwrap_or(0)
    }
    pub fn cur_chat(&self) -> &Chat {
        &self.chats[self.cur_idx()]
    }
    fn cur_chat_mut(&mut self) -> &mut Chat {
        let i = self.cur_idx();
        &mut self.chats[i]
    }
    pub fn sel_id(&self) -> Option<u64> {
        let order = self.visible_order();
        order.get(self.sidebar_cursor).map(|&i| self.chats[i].id)
    }
    pub fn is_open(&self, id: u64) -> bool {
        self.panes.iter().any(|p| p.tabs.contains(&id))
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

    fn category_indices(&self) -> Vec<usize> {
        let q = self.cur_chat().qualifier.clone();
        self.visible_order()
            .into_iter()
            .filter(|&i| self.chats[i].qualifier == q)
            .collect()
    }

    fn sidebar_to(&mut self, id: u64) {
        let order = self.visible_order();
        if let Some(pos) = order.iter().position(|&i| self.chats[i].id == id) {
            self.sidebar_cursor = pos;
        }
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
            self.help_open = false;
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Command => self.key_command(k, ctrl),
            Mode::Rename => self.key_rename(k, ctrl),
            Mode::Insert => self.key_insert(k, ctrl),
            Mode::Picker => self.key_picker(k, ctrl),
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
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => self.send_prompt(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
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

    fn picker_down(&mut self) {
        let n = self.picker_candidates().len();
        if n > 0 {
            self.picker_sel = (self.picker_sel + 1).min(n - 1);
        }
    }
    fn picker_up(&mut self) {
        self.picker_sel = self.picker_sel.saturating_sub(1);
    }

    fn key_normal(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('h') if ctrl => self.focus_dir(Dir::Left),
            KeyCode::Char('l') if ctrl => self.focus_dir(Dir::Right),
            KeyCode::Char('j') if ctrl => self.focus_dir(Dir::Down),
            KeyCode::Char('k') if ctrl => self.focus_dir(Dir::Up),
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
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('H') => self.pane_tab_cycle(-1),
            KeyCode::Char('L') => self.pane_tab_cycle(1),
            KeyCode::Char('g') => self.pending = Pending::G,
            KeyCode::Char('G') => self.cur_chat_mut().follow = true,
            KeyCode::Char('n') => self.new_chat(),
            KeyCode::Char('a') if self.focus == Focus::Sidebar => self.new_named_chat(),
            KeyCode::Char('r') => self.rename_start(),
            KeyCode::Char('d') if self.focus == Focus::Sidebar => {
                if let Some(id) = self.sel_id() {
                    self.delete_chat(id);
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
            KeyCode::Char('h') | KeyCode::Left => self.focus_dir(Dir::Left),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                if self.focus == Focus::Sidebar {
                    self.open_selected()
                } else {
                    self.focus_dir(Dir::Right)
                }
            }
            KeyCode::Tab => self.pane_tab_cycle(1),
            KeyCode::BackTab => self.pane_tab_cycle(-1),
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
                    KeyCode::Char('t') => self.pane_tab_cycle(1),
                    KeyCode::Char('T') => self.pane_tab_cycle(-1),
                    _ => {}
                }
            }
            Pending::Leader => match k.code {
                KeyCode::Char('e') => {
                    self.pending = Pending::None;
                    self.toggle_sidebar();
                }
                KeyCode::Char('z') => self.pending = Pending::LeaderZ,
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
            Pending::LeaderS => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('v') => {
                        self.split(SplitDir::V);
                        self.open_picker(PickerAction::OpenHere);
                    }
                    KeyCode::Char('h') => {
                        self.split(SplitDir::H);
                        self.open_picker(PickerAction::OpenHere);
                    }
                    KeyCode::Char('c') => self.open_picker(PickerAction::AddTab),
                    KeyCode::Char('x') => self.close_pane(),
                    KeyCode::Char('d') => self.detach_tab(),
                    KeyCode::Char('o') => self.only_pane(),
                    KeyCode::Char('m') => self.zoom = !self.zoom,
                    _ => {}
                }
            }
            Pending::LeaderT => {
                self.pending = Pending::None;
                match k.code {
                    KeyCode::Char('o') | KeyCode::Char('f') => self.new_chat(),
                    KeyCode::Char('x') => self.delete_chat(self.cur_id()),
                    KeyCode::Char('n') => self.pane_tab_cycle(1),
                    KeyCode::Char('p') => self.pane_tab_cycle(-1),
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

    // ---- focus / panes ----

    fn focus_sidebar(&mut self) {
        self.focus = Focus::Sidebar;
        self.sidebar_open = true;
        self.sidebar_to(self.cur_id());
    }

    /// Space e: toggle sidebar visibility. Focus is handled by Ctrl-h / Ctrl-l.
    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if !self.sidebar_open && self.focus == Focus::Sidebar {
            self.focus = Focus::Main;
        } else if self.sidebar_open {
            self.sidebar_to(self.cur_id());
        }
    }

    fn focus_dir(&mut self, dir: Dir) {
        if self.focus == Focus::Sidebar {
            if let Dir::Right = dir {
                self.focus = Focus::Main;
            }
            return;
        }
        // focus == Main
        let n = self.panes.len();
        let cur = self.active_pane;
        // returns Some(pane) to move to, or None; special: usize::MAX => sidebar
        let target: Option<usize> = if n == 1 {
            match dir {
                Dir::Left => Some(usize::MAX),
                _ => None,
            }
        } else if n == 2 {
            match (self.split_dir, dir) {
                (SplitDir::V, Dir::Left) => Some(if cur == 1 { 0 } else { usize::MAX }),
                (SplitDir::V, Dir::Right) => {
                    if cur == 0 {
                        Some(1)
                    } else {
                        None
                    }
                }
                (SplitDir::H, Dir::Up) => {
                    if cur == 1 {
                        Some(0)
                    } else {
                        None
                    }
                }
                (SplitDir::H, Dir::Down) => {
                    if cur == 0 {
                        Some(1)
                    } else {
                        None
                    }
                }
                (SplitDir::H, Dir::Left) => Some(usize::MAX),
                _ => None,
            }
        } else {
            // grid: TL=0 TR=1 BL=2 BR=3
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
            Some(p) if p < n => self.active_pane = p,
            _ => {}
        }
    }

    fn pane_tab_cycle(&mut self, d: isize) {
        let p = &mut self.panes[self.active_pane];
        let n = p.tabs.len() as isize;
        if n < 2 {
            return;
        }
        p.active_tab = (p.active_tab as isize + d).rem_euclid(n) as usize;
    }

    fn split(&mut self, dir: SplitDir) {
        if self.panes.len() >= 4 {
            return;
        }
        let cur = self.cur_id();
        self.panes.push(Pane {
            tabs: vec![cur],
            active_tab: 0,
        });
        if self.panes.len() == 2 {
            self.split_dir = dir;
        }
        self.active_pane = self.panes.len() - 1;
        self.zoom = false;
        self.focus = Focus::Main;
    }

    fn detach_tab(&mut self) {
        if self.panes.len() >= 4 {
            return;
        }
        let (id, keep) = {
            let p = &mut self.panes[self.active_pane];
            if p.tabs.len() <= 1 {
                return; // nothing to separate
            }
            let id = p.tabs.remove(p.active_tab);
            if p.active_tab >= p.tabs.len() {
                p.active_tab = p.tabs.len() - 1;
            }
            (id, true)
        };
        if keep {
            self.panes.push(Pane {
                tabs: vec![id],
                active_tab: 0,
            });
            if self.panes.len() == 2 {
                self.split_dir = SplitDir::V;
            }
            self.active_pane = self.panes.len() - 1;
            self.zoom = false;
        }
    }

    fn close_pane(&mut self) {
        if self.panes.len() <= 1 {
            return;
        }
        self.panes.remove(self.active_pane);
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        }
        self.zoom = false;
    }

    fn only_pane(&mut self) {
        let keep = Pane {
            tabs: self.panes[self.active_pane].tabs.clone(),
            active_tab: self.panes[self.active_pane].active_tab,
        };
        self.panes = vec![keep];
        self.active_pane = 0;
        self.zoom = false;
    }

    /// Add a chat as a new inter-tab in the focused pane (or switch to it if
    /// already there). This is the `Space s c` merge.
    fn pane_add_tab(&mut self, id: u64) {
        let p = &mut self.panes[self.active_pane];
        if let Some(pos) = p.tabs.iter().position(|&t| t == id) {
            p.active_tab = pos;
        } else {
            p.tabs.push(id);
            p.active_tab = p.tabs.len() - 1;
        }
    }

    /// Open a chat in the focused pane's current tab (replace), or switch if it
    /// is already a tab there.
    fn pane_open(&mut self, id: u64) {
        let p = &mut self.panes[self.active_pane];
        if let Some(pos) = p.tabs.iter().position(|&t| t == id) {
            p.active_tab = pos;
        } else if !p.tabs.is_empty() {
            let at = p.active_tab.min(p.tabs.len() - 1);
            p.tabs[at] = id;
        } else {
            p.tabs.push(id);
            p.active_tab = 0;
        }
    }

    fn open_selected(&mut self) {
        if let Some(id) = self.sel_id() {
            self.pane_open(id);
            self.focus = Focus::Main;
        }
    }

    // ---- picker (Space s c) ----

    fn open_picker(&mut self, action: PickerAction) {
        self.picker_action = action;
        self.picker_query.clear();
        self.picker_sel = 0;
        self.mode = Mode::Picker;
    }

    pub fn picker_candidates(&self) -> Vec<usize> {
        let q = self.picker_query.to_lowercase();
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                q.is_empty() || {
                    let t = if c.title.trim().is_empty() {
                        "untitled".to_string()
                    } else {
                        c.title.to_lowercase()
                    };
                    t.contains(&q)
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn picker_commit(&mut self) {
        let cands = self.picker_candidates();
        if let Some(&ci) = cands.get(self.picker_sel) {
            let id = self.chats[ci].id;
            match self.picker_action {
                PickerAction::AddTab => self.pane_add_tab(id),
                PickerAction::OpenHere => self.pane_open(id),
            }
            self.focus = Focus::Main;
        }
        self.mode = Mode::Normal;
    }

    // ---- sidebar / jump ----

    fn sidebar_move(&mut self, delta: isize) {
        let order = self.visible_order();
        if order.is_empty() {
            return;
        }
        let cur = self.sidebar_cursor.min(order.len() - 1) as isize;
        let next = (cur + delta).clamp(0, order.len() as isize - 1) as usize;
        self.sidebar_cursor = next;
    }

    fn leader_jump(&mut self, d: char) {
        let idx = (d as u8 - b'0') as usize;
        let cat = self.category_indices();
        if let Some(&ci) = cat.get(idx) {
            let id = self.chats[ci].id;
            self.pane_open(id);
            self.sidebar_to(id);
        }
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

    // ---- chat lifecycle ----

    fn add_chat_to_list(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.chats.push(Chat::fresh(id));
        self.sidebar_to(id);
        self.persist();
        id
    }

    fn new_chat(&mut self) {
        let id = self.add_chat_to_list();
        self.pane_open(id);
        self.focus = Focus::Main;
        self.mode = Mode::Insert;
    }

    /// Add a chat and name it inline in the sidebar (focus stays on the sidebar,
    /// entry is unnamed). Not opened in a pane until you Enter it.
    fn new_named_chat(&mut self) {
        self.add_chat_to_list();
        self.sidebar_open = true;
        self.focus = Focus::Sidebar;
        self.rename_buf.clear();
        self.mode = Mode::Rename;
    }

    fn rename_start(&mut self) {
        let id = if self.focus == Focus::Sidebar {
            self.sel_id()
        } else {
            Some(self.cur_id())
        };
        let Some(id) = id else {
            return;
        };
        self.sidebar_open = true;
        self.focus = Focus::Sidebar;
        self.sidebar_to(id);
        let title = self
            .chat_index(id)
            .map(|i| self.chats[i].title.clone())
            .unwrap_or_default();
        self.rename_buf = title;
        self.mode = Mode::Rename;
    }

    fn rename_commit(&mut self) {
        let Some(id) = self.sel_id() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(idx) = self.chat_index(id) else {
            self.mode = Mode::Normal;
            return;
        };
        let name = self.rename_buf.trim().to_string();
        if name.is_empty() {
            let auto = self.chats[idx].transcript.iter().find_map(|e| match e {
                Entry::User(t) => Some(slug(t)),
                _ => None,
            });
            if let Some(a) = auto {
                self.chats[idx].title = a;
                self.chats[idx].autonamed = true;
            }
        } else {
            self.chats[idx].title = name;
            self.chats[idx].autonamed = true;
        }
        self.mode = Mode::Normal;
        self.persist();
    }

    fn delete_chat(&mut self, id: u64) {
        if self.chats.len() <= 1 {
            return;
        }
        if let Some(ci) = self.chat_index(id) {
            self.chats.remove(ci);
        }
        let mut i = 0;
        while i < self.panes.len() {
            let p = &mut self.panes[i];
            p.tabs.retain(|&t| t != id);
            if p.active_tab >= p.tabs.len() {
                p.active_tab = p.tabs.len().saturating_sub(1);
            }
            if p.tabs.is_empty() {
                self.panes.remove(i);
            } else {
                i += 1;
            }
        }
        if self.panes.is_empty() {
            let fid = self.chats[0].id;
            self.panes.push(Pane {
                tabs: vec![fid],
                active_tab: 0,
            });
        }
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        }
        let order = self.visible_order();
        if self.sidebar_cursor >= order.len() {
            self.sidebar_cursor = order.len().saturating_sub(1);
        }
        self.persist();
    }

    fn set_qualifier(&mut self, q: Option<String>) {
        let id = self.cur_id();
        if let Some(i) = self.chat_index(id) {
            self.chats[i].qualifier = q;
        }
        self.sidebar_to(id);
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
            "close" => self.delete_chat(self.cur_id()),
            "vsplit" | "vs" => self.split(SplitDir::V),
            "split" | "sp" => self.split(SplitDir::H),
            "only" => self.only_pane(),
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
            .map(|(_, c)| {
                if c.title.trim().is_empty() {
                    "untitled"
                } else {
                    c.title.as_str()
                }
            })
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
            if chat.title.trim().is_empty() { "untitled" } else { &chat.title },
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
        if prompt.starts_with("/loop") {
            self.set_qualifier(Some("loop".into()));
            self.cur_chat_mut()
                .transcript
                .push(Entry::Note("marked as loop — scheduler coming soon".into()));
            self.input.clear();
            self.mode = Mode::Normal;
            return;
        }

        let dangerous = self.dangerous;
        let model = self.model_cli.clone();
        let tx = self.tx.clone();
        let idx = self.cur_idx();
        let sysp = self.env_prompt(idx);

        let spec = {
            let c = &mut self.chats[idx];
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
