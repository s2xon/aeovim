//! App state + update logic — two-level model.
//!
//! A **Space** is a named container of 1–4 **Chats**. The sidebar lists spaces;
//! the active space renders its chats as split panes. Every chat belongs to
//! exactly one space; deleting a space's last chat deletes the space. Spaces can
//! be merged (chats combined, ≤4) and a chat can be popped out into its own space.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

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

    fn from_persist(id: u64, pc: &PersistChat) -> Self {
        let mut transcript = Vec::new();
        transcript.push(Entry::Note(format!(
            "resumed · session {} (send a message to continue)",
            &pc.session_id[..8.min(pc.session_id.len())]
        )));
        Chat {
            id,
            title: pc.title.clone(),
            autonamed: true,
            transcript,
            streaming: None,
            in_flight: false,
            session_id: pc.session_id.clone(),
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

pub struct Space {
    pub id: u64,
    pub name: String,
    pub chats: Vec<Chat>,
    pub focused: usize,
    pub split_dir: SplitDir,
    pub zoom: bool,
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
    // A single-chat space's name IS its chat's name (renaming the chat renames
    // the entry). Multi-chat spaces use their own name.
    if sp.chats.len() == 1 {
        chat_title(&sp.chats[0])
    } else if !sp.name.trim().is_empty() {
        sp.name.clone()
    } else {
        sp.chats
            .first()
            .map(chat_title)
            .unwrap_or_else(|| "space".to_string())
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
    Pipe { to: String, from: String, message: String },
}

pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub pending: Pending,
    pub input: String,
    pub cmd: String,
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
    pub model_cli: Option<String>,
    pub model_display: String,
    pub dangerous: bool,
    pub should_quit: bool,
    pub spinner: usize,
    pub help_open: bool,
    next_chat_id: u64,
    next_space_id: u64,
    chat_counter: u64,
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
        let mut chat_counter = 1u64;

        for ps in &restored {
            let mut chats: Vec<Chat> = Vec::new();
            for pc in ps.chats.iter().take(4) {
                chats.push(Chat::from_persist(next_chat_id, pc));
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
            });
            next_space_id += 1;
        }
        if spaces.is_empty() {
            let mut c = Chat::fresh(next_chat_id);
            next_chat_id += 1;
            c.title = format!("chat{chat_counter}");
            chat_counter += 1;
            spaces.push(Space::one(next_space_id, c));
            next_space_id += 1;
        }

        Self {
            mode: Mode::Normal,
            focus: Focus::Main,
            pending: Pending::None,
            input: String::new(),
            cmd: String::new(),
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
            model_cli,
            model_display,
            dangerous,
            should_quit: false,
            spinner: 0,
            help_open: false,
            next_chat_id,
            next_space_id,
            chat_counter,
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
                chats: sp
                    .chats
                    .iter()
                    .map(|c| PersistChat {
                        title: c.title.clone(),
                        session_id: c.session_id.clone(),
                        cost: c.cost,
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
            Msg::Input(_) => {}
            Msg::Pipe { to, from, message } => self.inject_pipe(to, from, message),
            Msg::Agent { chat, ev } => self.handle_agent(chat, ev),
            Msg::TurnEnded { chat, error } => {
                if let Some(c) = self.chat_by_id_mut(chat) {
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
        if let Some(c) = self.chat_by_id_mut(id) {
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

    fn key_confirm(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
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
            KeyCode::Char('c') if ctrl => self.should_quit = true,
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
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('H') => self.pane_cycle(-1),
            KeyCode::Char('L') => self.pane_cycle(1),
            KeyCode::Char('g') => self.pending = Pending::G,
            KeyCode::Char('G') => self.cur_chat_mut().follow = true,
            KeyCode::Char('n') => {
                self.new_space();
                self.focus = Focus::Main;
                self.mode = Mode::Insert;
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
            KeyCode::Right | KeyCode::Enter => {
                if self.focus == Focus::Sidebar {
                    self.activate_selected()
                } else {
                    self.focus_dir(Dir::Right)
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

    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.focus = Focus::Sidebar;
            self.sidebar_cursor = self.active_space;
        } else {
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
        format!("chat{n}")
    }

    fn new_space(&mut self) -> usize {
        let cid = self.next_chat_id;
        self.next_chat_id += 1;
        let sid = self.next_space_id;
        self.next_space_id += 1;
        let title = self.next_chat_title();
        let mut c = Chat::fresh(cid);
        c.title = title;
        self.spaces.push(Space::one(sid, c));
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
        self.spaces.push(Space::one(sid, chat));
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
                let name = self.rename_buf.trim().to_string();
                if self.spaces[idx].chats.len() == 1 {
                    // single-chat space: renaming the entry renames the chat
                    if !name.is_empty() {
                        self.spaces[idx].chats[0].title = name;
                        self.spaces[idx].chats[0].autonamed = true;
                    }
                } else {
                    self.spaces[idx].name = name;
                }
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
        match cmd.as_str() {
            "q" | "quit" => {
                self.should_quit = true;
                return;
            }
            "new" => {
                self.new_space();
                self.mode = Mode::Insert;
                return;
            }
            "w" | "ws" | "write" => {
                self.persist();
                return;
            }
            _ => {}
        }
        if self.spaces.is_empty() {
            return;
        }
        match cmd.as_str() {
            "close" => self.close_focused_pane(),
            "pop" => self.pop_chat(),
            "vsplit" | "vs" => self.spaces[self.active_space].split_dir = SplitDir::V,
            "split" | "sp" => self.spaces[self.active_space].split_dir = SplitDir::H,
            _ => {}
        }
    }

    fn env_prompt_for(&self, si: usize, ci: usize) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
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
        let model = self.model_cli.clone();
        let tx = self.tx.clone();
        let sysp = self.env_prompt_for(si, ci);
        let sname = space_name(&self.spaces[si]);
        let pipe = std::env::var("AEOVIM_PIPE").ok();
        let spec = {
            let c = &mut self.spaces[si].chats[ci];
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
                space_name: sname,
                pipe_path: pipe,
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
        if self.spaces[ai].chats[fi].in_flight {
            return;
        }
        // Name the chat instantly from the first prompt — a local text slug, no
        // claude call involved.
        if !self.spaces[ai].chats[fi].autonamed {
            self.spaces[ai].chats[fi].title = slug(&prompt);
            self.spaces[ai].chats[fi].autonamed = true;
        }
        self.input.clear();
        self.mode = Mode::Normal;
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
        if self.spaces[si].chats[ci].in_flight {
            self.spaces[si].chats[ci]
                .transcript
                .push(Entry::Note(format!("pipe from {who} (queued — busy): {message}")));
            return;
        }
        let prompt = format!("[message from space \"{who}\" via aeovim pipe]\n{message}");
        self.spawn_for(si, ci, prompt);
    }
}
