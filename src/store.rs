//! Session persistence, keyed per tmux session.
//!
//! We save lightweight chat metadata (title, qualifier, claude session id, cost)
//! to `~/.local/state/aeovim/<tmux-session>.json`. On relaunch, chats are
//! restored and continue via `claude --resume <session-id>`, so the agent
//! context is intact even though the visible transcript starts fresh.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct PersistChat {
    pub title: String,
    #[serde(default)]
    pub qualifier: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub cost: f64,
}

fn state_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/aeovim"))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Identify the workspace: the tmux session name, else "default".
pub fn workspace_key() -> String {
    if std::env::var("TMUX").is_ok() {
        if let Ok(out) = std::process::Command::new("tmux")
            .args(["display-message", "-p", "#S"])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return sanitize(&s);
                }
            }
        }
    }
    "default".into()
}

fn file(key: &str) -> Option<PathBuf> {
    Some(state_dir()?.join(format!("{key}.json")))
}

pub fn load(key: &str) -> Vec<PersistChat> {
    let Some(p) = file(key) else { return vec![] };
    let Ok(data) = std::fs::read_to_string(&p) else { return vec![] };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save(key: &str, chats: &[PersistChat]) {
    let Some(dir) = state_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Some(p) = file(key) {
        if let Ok(data) = serde_json::to_string_pretty(chats) {
            let _ = std::fs::write(p, data);
        }
    }
}
