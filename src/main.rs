//! aeovim — a modal TUI for orchestrating coding agents. Binary: `avim`.
//!
//! Walking skeleton: wraps the `claude` CLI over headless stream-json and renders
//! streamed replies into a multi-chat, sidebar-organized shell. Keymap + theme
//! are ported from the user's Neovim config (leader = Space, Ctrl-hjkl panes,
//! nvim-tree sidebar keys, harpoon number-jump, lilac palette).

mod agent;
mod app;
mod protocol;
mod store;
mod theme;
mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use app::{App, Msg};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    // Debug affordance: parse a captured stream-json file without a TTY.
    if let Some(pos) = args.iter().position(|a| a == "--replay") {
        let file = args.get(pos + 1).cloned().unwrap_or_default();
        let data = std::fs::read_to_string(&file)?;
        for line in data.lines() {
            for ev in protocol::parse_line(line) {
                println!("{ev:?}");
            }
        }
        return Ok(());
    }

    // Flags. Dangerous permissions by default (matches the user's claude alias).
    let mut model_cli: Option<String> = None;
    let mut dangerous = true;
    // Mouse capture off by default so the cursor can select/copy transcript text
    // (and tmux/Ghostty native selection works). `--mouse` starts it on for
    // wheel-scroll; toggle at runtime with `:mouse`.
    let mut mouse_start = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" | "-m" => {
                i += 1;
                model_cli = args.get(i).cloned();
            }
            "--safe" => dangerous = false,
            "--mouse" => mouse_start = true,
            _ => {}
        }
        i += 1;
    }

    // Claim the workspace key with a pid lock: two avims in one tmux session
    // used to share a state file and silently clobber each other on save.
    let key = store::claim_key(&store::workspace_key());
    // Every saved chat is adopted into the sessions sidebar (old multi-space
    // files flatten into it too — nothing is lost).
    let (restored, load_warning) = store::load(&key);

    install_panic_hook();
    enable_raw_mode()?;
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    execute!(stdout(), EnterAlternateScreen)?;
    // Bracketed paste: a Cmd-V of multi-line text arrives as ONE Event::Paste
    // (newlines intact) instead of a burst of keystrokes where the first '\n'
    // would fire send. Kept independent of mouse capture.
    execute!(stdout(), EnableBracketedPaste)?;
    if mouse_start {
        execute!(stdout(), EnableMouseCapture)?;
    }
    if enhanced {
        // Disambiguate Ctrl-h from Backspace etc. (Ghostty/kitty protocol).
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let (tx, rx) = mpsc::unbounded_channel::<Msg>();

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut es = EventStream::new();
            while let Some(Ok(ev)) = es.next().await {
                if tx.send(Msg::Input(ev)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(120));
            loop {
                iv.tick().await;
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(model_cli, dangerous, tx.clone(), key.clone(), restored);
    app.mouse_capture = mouse_start;
    app.toast = load_warning;
    let res = run(&mut terminal, &mut app, rx).await;
    app.persist();
    // Stop every live claude child — quitting must never leave agents running
    // (and editing files) invisibly. kill_on_drop is the backstop; this is the
    // deliberate path.
    app.kill_all_sessions();
    store::release_key(&key);

    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    res
}

/// The event loop. Three rules keep it responsive under streaming load:
/// 1. Drain everything already queued before drawing — a burst of 100 token
///    deltas becomes ONE redraw, and keystrokes never wait behind them.
/// 2. Input draws immediately; agent-only changes are capped at ~30fps.
/// 3. The tick (spinner/elapsed) only redraws while something is in flight.
///
/// The old loop drew the full screen once per message — one draw per token —
/// which made typing lag grow with reply length.
async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    mut rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    const STREAM_FRAME: Duration = Duration::from_millis(33);
    let mut last_draw = std::time::Instant::now();
    terminal.draw(|f| ui::render(f, app))?;
    'outer: while let Some(msg) = rx.recv().await {
        let mut had_input = matches!(msg, Msg::Input(_) | Msg::Pasted(_));
        let mut had_tick = matches!(msg, Msg::Tick);
        app.handle(msg);
        while let Ok(m) = rx.try_recv() {
            had_input |= matches!(m, Msg::Input(_) | Msg::Pasted(_));
            had_tick |= matches!(m, Msg::Tick);
            app.handle(m);
            if app.should_quit {
                break 'outer;
            }
        }
        if app.should_quit {
            break;
        }
        let stream_due = app.dirty && last_draw.elapsed() >= STREAM_FRAME;
        let tick_due = had_tick && (app.any_in_flight() || app.dirty);
        if had_input || stream_due || tick_due {
            terminal.draw(|f| ui::render(f, app))?;
            app.dirty = false;
            last_draw = std::time::Instant::now();
        }
    }
    Ok(())
}

fn install_panic_hook() {
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        orig(info);
    }));
}

fn print_help() {
    println!("aeovim — a modal TUI for orchestrating coding agents");
    println!("command: avim   (project: aeovim, like neovim -> nvim)\n");
    println!("USAGE:");
    println!("  avim [--model <name>] [--safe] [--mouse]");
    println!("  avim --replay <stream-json-file>   # debug: dump parsed events\n");
    println!("PERMISSIONS: dangerous by default (--dangerously-skip-permissions).");
    println!("             pass --safe to use --permission-mode acceptEdits.\n");
    println!("MOUSE: off by default so you can select/copy transcript text with the");
    println!("       cursor (tmux/Ghostty selection). --mouse (or /mouse) turns on");
    println!("       wheel-scroll, at the cost of drag-select needing Shift/Option.\n");
    println!("SPACES + MODAL — a space holds 1-2 chats; sidebar left, Insert on launch.");
    println!("KEYS:");
    println!("  type + Enter     send (stays Insert)      Shift/Alt-Enter / Ctrl-j  newline");
    println!("  Esc              Insert -> Normal · Normal: snap to the live tail");
    println!("  Ctrl-c           interrupt running turn (again = force) · idle: quit (asks)");
    println!("  Space ee         toggle spaces sidebar     Space 1-0 / ga / gt  jump space");
    println!("  Space n          new space                 Space t / :tasks     tasks");
    println!("  :vs / Ctrl-w v   split: second chat        Ctrl-h/l / Ctrl-w    move panes");
    println!("  :q               close pane, then space    :qa quit · :q! now");
    println!("  :diff            diff pad (j/k scroll, Ctrl-j older, Ctrl-k newer)");
    println!("  :cost / :status  spend + session info (kept off the chrome)");
    println!("  Ctrl-t           expand/collapse tool output     ? / F1  keys cheatsheet");
    println!("  THEME: colour-agnostic — override tokens in ~/.config/aeovim/theme.toml");
    println!("  spaces persist per tmux session — relaunch avim to resume them");
}
