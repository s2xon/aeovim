//! Spawning a `claude` turn as a child process and streaming its events back
//! to the app, tagged with the originating chat id.
//!
//! Skeleton model: **one child per turn** (`claude -p "<prompt>" ...`), stdin
//! nulled so there is no "no stdin data" stall. Turn one uses `--session-id`;
//! follow-ups use `--resume <id>` to keep conversation context. Permissions
//! default to `--dangerously-skip-permissions` (see `TurnSpec::dangerous`).

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::Msg;
use crate::protocol::{parse_line, AgentEvent};

pub struct TurnSpec {
    pub chat: u64,
    pub prompt: String,
    pub session_id: String,
    pub first: bool,
    pub model: Option<String>,
    pub dangerous: bool,
    pub permission_mode: String,
}

/// Resolve the claude binary. `Command` execs by PATH lookup and ignores shell
/// aliases, so the interactive `claude -> claude --dangerously-skip-permissions`
/// alias does not apply here. Override with `AVIM_CLAUDE_BIN`.
fn claude_bin() -> String {
    std::env::var("AVIM_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

/// Spawn a turn on a background task. Emits `Msg::Agent { chat, ev }` per event
/// and a final `Msg::TurnEnded { chat, .. }` when the child exits.
pub fn spawn_turn(spec: TurnSpec, tx: UnboundedSender<Msg>) {
    tokio::spawn(async move {
        let mut cmd = Command::new(claude_bin());
        cmd.arg("-p")
            .arg(&spec.prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose") // mandatory with -p + stream-json
            .arg("--include-partial-messages"); // token-level deltas

        if spec.dangerous {
            cmd.arg("--dangerously-skip-permissions");
        } else {
            cmd.arg("--permission-mode").arg(&spec.permission_mode);
        }
        if let Some(model) = &spec.model {
            cmd.arg("--model").arg(model);
        }
        if spec.first {
            cmd.arg("--session-id").arg(&spec.session_id);
        } else {
            cmd.arg("--resume").arg(&spec.session_id);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::TurnEnded {
                    chat: spec.chat,
                    error: Some(format!("failed to spawn claude: {e}")),
                });
                return;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match parse_line(&line) {
                AgentEvent::Ignore => {}
                ev => {
                    if tx.send(Msg::Agent { chat: spec.chat, ev }).is_err() {
                        return; // app gone
                    }
                }
            }
        }

        let mut errbuf = String::new();
        let mut errlines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = errlines.next_line().await {
            errbuf.push_str(&l);
            errbuf.push('\n');
        }

        let error = match child.wait().await {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("claude exited ({status}). {}", errbuf.trim())),
            Err(e) => Some(format!("waiting on claude failed: {e}")),
        };

        let _ = tx.send(Msg::TurnEnded { chat: spec.chat, error });
    });
}
