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
    /// Whether this chat ever actually sent a turn — decides `--resume` vs a
    /// fresh `--session-id` on relaunch (resuming a never-created session is a
    /// guaranteed error). Older files lack the field; assume started, since the
    /// self-heal path covers a wrong guess.
    #[serde(default = "default_true")]
    pub started: bool,
    /// Full transcript so reopening a session shows the prior text, not a blank.
    /// `default` keeps older state files (which lack it) loadable.
    #[serde(default)]
    pub transcript: Vec<crate::app::Entry>,
}

fn default_true() -> bool {
    true
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

/// Load saved spaces. A corrupt state file is backed up (`<key>.json.corrupt`)
/// and reported instead of being silently replaced with nothing — the old
/// `unwrap_or_default()` here erased every space on any parse hiccup.
pub fn load(key: &str) -> (Vec<PersistSpace>, Option<String>) {
    let Some(p) = file(key) else { return (vec![], None) };
    let Ok(data) = std::fs::read_to_string(&p) else { return (vec![], None) };
    match serde_json::from_str(&data) {
        Ok(v) => (v, None),
        Err(e) => {
            let backup = p.with_extension("json.corrupt");
            let _ = std::fs::rename(&p, &backup);
            (
                vec![],
                Some(format!(
                    "state file was corrupt ({e}) — backed up to {}",
                    backup.display()
                )),
            )
        }
    }
}

pub fn save(key: &str, spaces: &[PersistSpace]) {
    let Some(dir) = state_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Some(p) = file(key) {
        if spaces.is_empty() {
            // Everything was deleted — remove the file so it cleans up nicely
            // instead of leaving an empty "[]" behind.
            let _ = std::fs::remove_file(&p);
            return;
        }
        if let Ok(data) = serde_json::to_string_pretty(spaces) {
            // Atomic: write a temp file, then rename over the target. A crash
            // mid-write can no longer truncate the only copy of the state.
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, data).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

/// Claim a workspace key for this process. If another live avim already holds
/// `base` (same tmux session), fall back to `base-2`, `base-3`, … so two
/// instances can never clobber each other's state file (last-writer used to
/// win, silently destroying the other's spaces).
pub fn claim_key(base: &str) -> String {
    let Some(dir) = state_dir() else { return base.to_string() };
    let _ = std::fs::create_dir_all(&dir);
    let pid_alive = |pid: &str| {
        std::process::Command::new("ps")
            .args(["-p", pid])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    for n in 0..10u32 {
        let key = if n == 0 {
            base.to_string()
        } else {
            format!("{base}-{}", n + 1)
        };
        let lock = dir.join(format!("{key}.lock"));
        let held = std::fs::read_to_string(&lock)
            .ok()
            .map(|pid| {
                let pid = pid.trim().to_string();
                pid != std::process::id().to_string() && pid_alive(&pid)
            })
            .unwrap_or(false);
        if !held {
            let _ = std::fs::write(&lock, std::process::id().to_string());
            return key;
        }
    }
    base.to_string()
}

pub fn release_key(key: &str) {
    if let Some(dir) = state_dir() {
        let _ = std::fs::remove_file(dir.join(format!("{key}.lock")));
    }
}

/// Preserve a multi-space state file (from the old build) before single-session
/// saves overwrite it. Only the first backup is kept. Returns a user notice.
pub fn backup_multi(key: &str) -> Option<String> {
    let p = file(key)?;
    let bak = p.with_extension("json.multi.bak");
    if bak.exists() {
        return None;
    }
    std::fs::copy(&p, &bak).ok()?;
    Some(format!("older chats preserved in {}", bak.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;

    #[test]
    fn transcript_round_trips() {
        let chat = PersistChat {
            title: "t".into(),
            session_id: "sid".into(),
            cost: 1.5,
            started: true,
            transcript: vec![
                Entry::User("hello".into()),
                Entry::Assistant("hi".into()),
                Entry::ToolResult { ok: true, text: "done".into() },
                Entry::Note("n".into()),
            ],
        };
        let spaces = vec![PersistSpace { name: "s".into(), chats: vec![chat] }];
        let json = serde_json::to_string(&spaces).unwrap();
        let back: Vec<PersistSpace> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].chats[0].transcript.len(), 4);
        assert_eq!(back[0].chats[0].title, "t");
    }

    #[test]
    fn legacy_file_without_transcript_loads() {
        // Older state files predate the transcript/started fields — must load,
        // and `started` must default to true (assume resumable).
        let json = r#"[{"name":"s","chats":[{"title":"t","session_id":"sid","cost":0.0}]}]"#;
        let back: Vec<PersistSpace> = serde_json::from_str(json).unwrap();
        assert!(back[0].chats[0].transcript.is_empty());
        assert!(back[0].chats[0].started);
    }

    #[test]
    fn save_writes_transcript_to_disk() {
        // Exercise the real save()/load() disk path the binary uses on :q.
        let key = "aeovim_selftest_persist";
        let spaces = vec![PersistSpace {
            name: "s".into(),
            chats: vec![PersistChat {
                title: "t".into(),
                session_id: "sid".into(),
                cost: 0.0,
                started: true,
                transcript: vec![Entry::User("hi".into()), Entry::Assistant("yo".into())],
            }],
        }];
        save(key, &spaces);
        let p = file(key).expect("state path");
        let raw = std::fs::read_to_string(&p).expect("state file written");
        assert!(raw.contains("\"transcript\""), "transcript missing on disk:\n{raw}");
        let (back, warn) = load(key);
        assert!(warn.is_none());
        assert_eq!(back[0].chats[0].transcript.len(), 2);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corrupt_state_is_backed_up_not_erased() {
        let key = "aeovim_selftest_corrupt";
        let p = file(key).expect("state path");
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        std::fs::write(&p, "{ this is not json").unwrap();
        let (back, warn) = load(key);
        assert!(back.is_empty());
        let warn = warn.expect("corruption must be reported");
        assert!(warn.contains("corrupt"));
        let backup = p.with_extension("json.corrupt");
        assert!(backup.exists(), "corrupt file must be preserved");
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn claim_key_avoids_live_lock() {
        // A lock held by a live pid (our own test process) forces a suffixed key.
        let dir = state_dir().unwrap();
        let _ = std::fs::create_dir_all(&dir);
        let base = "aeovim_selftest_lock";
        // Fake another live process holding the base lock: use pid 1 (launchd).
        std::fs::write(dir.join(format!("{base}.lock")), "1").unwrap();
        let key = claim_key(base);
        assert_ne!(key, base, "must not claim a lock held by a live pid");
        release_key(&key);
        let _ = std::fs::remove_file(dir.join(format!("{base}.lock")));
    }
}
