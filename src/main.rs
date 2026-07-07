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
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
            let ev = protocol::parse_line(line);
            if !matches!(ev, protocol::AgentEvent::Ignore) {
                println!("{ev:?}");
            }
        }
        return Ok(());
    }

    // Flags. Dangerous permissions by default (matches the user's claude alias).
    let mut model_cli: Option<String> = None;
    let mut dangerous = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" | "-m" => {
                i += 1;
                model_cli = args.get(i).cloned();
            }
            "--safe" => dangerous = false,
            _ => {}
        }
        i += 1;
    }

    let key = store::workspace_key();
    let restored = store::load(&key);

    install_panic_hook();
    enable_raw_mode()?;
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    execute!(stdout(), EnterAlternateScreen)?;
    if enhanced {
        // Disambiguate Ctrl-h from Backspace etc. (Ghostty/kitty protocol).
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let (tx, rx) = mpsc::unbounded_channel::<Msg>();

    // Inter-agent pipe: a FIFO the LLMs append JSON lines to, to message another
    // space (routed to that space's chat, visible in its transcript).
    let pipe_path = store::pipe_path(&key);
    if let Some(pp) = pipe_path.clone() {
        if let Some(dir) = pp.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&pp);
        let _ = std::process::Command::new("mkfifo").arg(&pp).status();
        std::env::set_var("AEOVIM_PIPE", &pp);
        let tx = tx.clone();
        std::thread::spawn(move || pipe_reader(&pp, tx));
    }

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

    let mut app = App::new(model_cli, dangerous, tx.clone(), key, restored);
    let res = run(&mut terminal, &mut app, rx).await;
    app.persist();

    if let Some(pp) = &pipe_path {
        let _ = std::fs::remove_file(pp);
    }
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    mut rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    terminal.draw(|f| ui::render(f, app))?;
    while let Some(msg) = rx.recv().await {
        let is_tick = matches!(msg, Msg::Tick);
        app.handle(msg);
        if app.should_quit {
            break;
        }
        if is_tick && !app.any_in_flight() {
            continue;
        }
        terminal.draw(|f| ui::render(f, app))?;
    }
    Ok(())
}

/// Blocking reader for the inter-agent FIFO. Reopens on each writer close.
fn pipe_reader(path: &std::path::Path, tx: mpsc::UnboundedSender<Msg>) {
    use std::io::BufRead;
    loop {
        match std::fs::File::open(path) {
            Ok(file) => {
                for line in std::io::BufReader::new(file).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        let s = |k: &str| {
                            v.get(k)
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string()
                        };
                        let (to, from, message) = (s("to"), s("from"), s("message"));
                        if !to.is_empty() && !message.is_empty() {
                            if tx.send(Msg::Pipe { to, from, message }).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

fn install_panic_hook() {
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        orig(info);
    }));
}

fn print_help() {
    println!("aeovim — a modal TUI for orchestrating coding agents");
    println!("command: avim   (project: aeovim, like neovim -> nvim)\n");
    println!("USAGE:");
    println!("  avim [--model <name>] [--safe]");
    println!("  avim --replay <stream-json-file>   # debug: dump parsed events\n");
    println!("PERMISSIONS: dangerous by default (--dangerously-skip-permissions).");
    println!("             pass --safe to use --permission-mode acceptEdits.\n");
    println!("KEYS (ported from your nvim; leader = Space):");
    println!("  i / Esc          compose / normal        Enter  send (in composer)");
    println!("  Ctrl-h/l         focus sidebar / chat     H / L  prev / next chat");
    println!("  Space e          toggle sidebar           Space zz   help / cheatsheet");
    println!("  Space 0-9        jump to chat N in group  Ctrl-h/l   focus left / right");
    println!("  (in sidebar) j/k move   a add+name   r rename   d close   Enter open");
    println!("  Space t o/x/n/p  new/close/next/prev chat");
    println!("  Space s ...      splits (coming next)     : command   q quit");
    println!("  sessions persist per tmux session — relaunch avim to resume");
}
