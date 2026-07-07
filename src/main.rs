//! aeovim — a modal TUI for orchestrating coding agents. Binary: `avim`.
//!
//! Walking skeleton: wraps the `claude` CLI over headless stream-json, renders
//! streamed replies into a multi-chat, sidebar-organized shell. Keybinds are
//! provisional; the real nvim-mirroring keymap comes later.

mod agent;
mod app;
mod protocol;
mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::EventStream;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
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

    install_panic_hook();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let (tx, rx) = mpsc::unbounded_channel::<Msg>();

    // Terminal input -> messages.
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
    // Spinner tick.
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

    let mut app = App::new(model_cli, dangerous, tx.clone());
    let res = run(&mut terminal, &mut app, rx).await;

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
        // On a bare tick, only redraw if something is streaming (keeps it idle-quiet).
        if is_tick && !app.chats.iter().any(|c| c.in_flight) {
            continue;
        }
        terminal.draw(|f| ui::render(f, app))?;
    }
    Ok(())
}

fn install_panic_hook() {
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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
    println!("KEYS (provisional — the real keymap comes later):");
    println!("  i          compose a prompt        Esc   back to normal");
    println!("  Enter      send                    q     quit");
    println!("  b          toggle sidebar          n     new chat");
    println!("  p / l      new parallel / loop chat");
    println!("  Tab/S-Tab  next / prev chat        gt/gT same");
    println!("  x          close chat -> Previous");
    println!("  j/k        scroll   C-d/C-u half page   gg/G top/bottom");
}
