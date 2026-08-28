//! One long-lived `claude` child per chat, driven over stdin stream-json.
//!
//! This replaces the old one-child-per-turn `claude -p "<prompt>" --resume`
//! model. A session is spawned lazily on the first send and stays alive across
//! turns, so follow-up turns skip the session-reload cost entirely, and the
//! control protocol gives us a real interrupt (verified live on 2.1.241: the
//! child acks `control_request{subtype:"interrupt"}`, the turn ends with
//! `result{subtype:"error_during_execution"}`, and the child keeps serving).
//!
//! Task layout per session: a writer owns stdin (fed by an mpsc of NDJSON
//! lines), a reader parses stdout into `Msg::Agent` events, a stderr reader
//! accumulates diagnostics, and a supervisor waits on the child (or a kill
//! signal) and reports `Msg::SessionEnded`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Notify;

use crate::app::Msg;
use crate::protocol::parse_line;

pub struct SessionSpec {
    pub chat: u64,
    pub session_id: String,
    /// false → mint a new session (`--session-id`); true → `--resume` it.
    pub resume: bool,
    pub model: Option<String>,
    pub dangerous: bool,
    pub permission_mode: String,
    /// Injected via --append-system-prompt so claude knows it runs in aeovim.
    pub append_system_prompt: Option<String>,
    /// Working directory for the child — the owning space's `dir`. Claude
    /// resolves relative paths and searches from here, so this is what makes
    /// a space actually "open into" a directory.
    pub cwd: PathBuf,
}

/// Handle the app keeps per chat while its session child is alive.
pub struct SessionHandle {
    stdin_tx: UnboundedSender<String>,
    kill: Arc<Notify>,
    killed: Arc<AtomicBool>,
    ctl_seq: std::cell::Cell<u64>,
}

impl SessionHandle {
    /// Queue one user turn onto the child's stdin. False if the session died.
    pub fn send_user_turn(&self, text: &str) -> bool {
        let line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        });
        self.stdin_tx.send(line.to_string()).is_ok()
    }

    /// Ask the child to interrupt the in-flight turn (keeps the child alive).
    pub fn send_interrupt(&self) -> bool {
        let n = self.ctl_seq.get() + 1;
        self.ctl_seq.set(n);
        let line = serde_json::json!({
            "type": "control_request",
            "request_id": format!("avim-int-{n}"),
            "request": { "subtype": "interrupt" },
        });
        self.stdin_tx.send(line.to_string()).is_ok()
    }

    /// Hard-stop: kill the child process. `SessionEnded` follows with no error.
    pub fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        self.kill.notify_one();
    }

    /// Best-effort liveness: false once the writer task has exited.
    pub fn is_alive(&self) -> bool {
        !self.stdin_tx.is_closed()
    }
}

/// Resolve the claude binary. `Command` execs by PATH lookup and ignores shell
/// aliases, so the interactive `claude -> claude --dangerously-skip-permissions`
/// alias does not apply here. Override with `AVIM_CLAUDE_BIN`.
fn claude_bin() -> String {
    std::env::var("AVIM_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

/// Spawn the session child and its service tasks. Events arrive as
/// `Msg::Agent { chat, ev }`; child death arrives as `Msg::SessionEnded`.
pub fn spawn_session(spec: SessionSpec, tx: UnboundedSender<Msg>) -> anyhow::Result<SessionHandle> {
    let mut cmd = Command::new(claude_bin());
    cmd.current_dir(&spec.cwd);
    cmd.arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose") // mandatory with -p + stream-json
        .arg("--include-partial-messages") // token-level deltas
        // AskUserQuestion can't be answered from headless stream-json — it
        // auto-resolves to empty selections with no TTY (claude-code#50728).
        // Disable it so the model asks in prose, which the chat can answer.
        .arg("--disallowedTools")
        .arg("AskUserQuestion");

    if let Some(sp) = &spec.append_system_prompt {
        cmd.arg("--append-system-prompt").arg(sp);
    }
    if spec.dangerous {
        cmd.arg("--dangerously-skip-permissions");
    } else {
        cmd.arg("--permission-mode").arg(&spec.permission_mode);
    }
    if let Some(model) = &spec.model {
        cmd.arg("--model").arg(model);
    }
    if spec.resume {
        cmd.arg("--resume").arg(&spec.session_id);
    } else {
        cmd.arg("--session-id").arg(&spec.session_id);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Belt-and-braces: if the handle/tasks are dropped (quit, panic), the
        // child dies with us instead of running on invisibly.
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("failed to spawn claude: {e}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    let kill = Arc::new(Notify::new());
    let killed = Arc::new(AtomicBool::new(false));

    // Writer: owns stdin; one NDJSON line per queued message.
    tokio::spawn(async move {
        while let Some(line) = stdin_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(b"\n").await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
        // stdin_rx closed (handle dropped) → stdin drops → child sees EOF and
        // exits cleanly (verified: exit 0 on stdin close).
    });

    // Reader: stdout lines → parsed events. Runs concurrently with the stderr
    // reader so a chatty stderr can never deadlock the pipes.
    {
        let tx = tx.clone();
        let chat = spec.chat;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        for ev in parse_line(&line) {
                            if tx.send(Msg::Agent { chat, ev }).is_err() {
                                return; // app gone
                            }
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(_) => break,   // read error (e.g. invalid UTF-8) — supervisor reports exit
                }
            }
        });
    }

    // Stderr: accumulate (bounded) so the supervisor can attach diagnostics.
    let errbuf = Arc::new(tokio::sync::Mutex::new(String::new()));
    {
        let errbuf = errbuf.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let mut b = errbuf.lock().await;
                if b.len() < 64 * 1024 {
                    b.push_str(&l);
                    b.push('\n');
                }
            }
        });
    }

    // Supervisor: wait for exit or a kill request; report SessionEnded.
    {
        let tx = tx.clone();
        let chat = spec.chat;
        let kill = kill.clone();
        let killed = killed.clone();
        let errbuf = errbuf.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // Give the stderr reader a beat to drain after exit.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let err = errbuf.lock().await.trim().to_string();
            let error = if killed.load(Ordering::SeqCst) {
                None // deliberate kill — not an error
            } else {
                match status {
                    Ok(s) if s.success() => None,
                    Ok(s) => Some(format!("claude exited ({s}). {err}")),
                    Err(e) => Some(format!("waiting on claude failed: {e}")),
                }
            };
            let _ = tx.send(Msg::SessionEnded { chat, error });
        });
    }

    Ok(SessionHandle {
        stdin_tx,
        kill,
        killed,
        ctl_seq: std::cell::Cell::new(0),
    })
}
