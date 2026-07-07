//! Persistence, keyed per tmux session. We save spaces (name + their chats'
//! titles/session ids), so relaunching restores the space layout and each chat
//! continues via `claude --resume`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct PersistChat {
    pub title: String,
    pub session_id: String,
    #[serde(default)]
    pub cost: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PersistSpace {
    #[serde(default)]
    pub name: String,
    pub chats: Vec<PersistChat>,
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

/// FIFO the LLMs write to in order to message another space.
pub fn pipe_path(key: &str) -> Option<PathBuf> {
    Some(state_dir()?.join(format!("{key}.pipe")))
}

pub fn load(key: &str) -> Vec<PersistSpace> {
    let Some(p) = file(key) else { return vec![] };
    let Ok(data) = std::fs::read_to_string(&p) else { return vec![] };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save(key: &str, spaces: &[PersistSpace]) {
    let Some(dir) = state_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Some(p) = file(key) {
        if let Ok(data) = serde_json::to_string_pretty(spaces) {
            let _ = std::fs::write(p, data);
        }
    }
}
