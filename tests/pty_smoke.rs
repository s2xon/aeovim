//! PTY + VT-emulation smoke tests (the CodeWhale-style harness): spawn the real
//! `avim` binary in a pseudo-terminal, drive it with keystrokes, feed its output
//! through a VT parser, and assert on the rendered screen. This catches the
//! class of bug unit tests can't see — escape-sequence corruption, mode wiring,
//! draw scheduling, real end-to-end streaming through a scripted fake `claude`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A fake `claude` speaking just enough stdin stream-json: per user turn it
/// emits init → text deltas → authoritative assistant text → a tool call and
/// its result → a closing message → result. "slowturn" streams for ~10s and
/// honors a control_request interrupt mid-stream (select on stdin — macOS's
/// bash 3.2 can't do fractional `read -t`, hence python). Exits on stdin close.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, select, sys, time

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def delta(text):
    emit({"type": "stream_event",
          "event": {"type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": text}}})

def result_ok(text):
    emit({"type": "result", "subtype": "success", "is_error": False,
          "total_cost_usd": 0.001, "result": text})

def result_interrupted():
    emit({"type": "result", "subtype": "error_during_execution",
          "is_error": True, "total_cost_usd": 0.0})

first = True
while True:
    line = sys.stdin.readline()
    if not line:
        break
    if "control_request" in line:
        emit({"type": "control_response",
              "response": {"subtype": "success", "request_id": "x"}})
        result_interrupted()
        continue
    if first:
        emit({"type": "system", "subtype": "init", "session_id": "fake-sid",
              "model": "fake-model", "slash_commands": ["/clear", "/model"]})
        first = False
    if "slowturn" in line:
        interrupted = False
        for _ in range(100):
            delta("tick ")
            r, _, _ = select.select([sys.stdin], [], [], 0.1)
            if r:
                nl = sys.stdin.readline()
                if "control_request" in nl:
                    emit({"type": "control_response",
                          "response": {"subtype": "success", "request_id": "x"}})
                    interrupted = True
                    break
        if interrupted:
            result_interrupted()
        else:
            emit({"type": "assistant",
                  "message": {"content": [{"type": "text", "text": "slow done"}]}})
            result_ok("slow done")
        continue
    delta("Hello ")
    time.sleep(0.05)
    delta("from fake.")
    time.sleep(0.05)
    emit({"type": "assistant",
          "message": {"content": [{"type": "text", "text": "Hello from fake."}]}})
    emit({"type": "assistant",
          "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash",
                                    "input": {"command": "echo hi"}}]}})
    emit({"type": "user",
          "message": {"content": [{"type": "tool_result", "tool_use_id": "t1",
                                    "content": "hi"}]}})
    emit({"type": "assistant",
          "message": {"content": [{"type": "tool_use", "id": "t2", "name": "Edit",
                                    "input": {"file_path": "demo.rs",
                                              "old_string": "a",
                                              "new_string": "b\nc"}}]}})
    emit({"type": "user",
          "message": {"content": [{"type": "tool_result", "tool_use_id": "t2",
                                    "content": "ok"}]}})
    # A SECOND file, so the diff pad has a real file boundary to scroll
    # across. Gated on the prompt: emitting it for every turn lengthens the
    # transcript and scrolls the other tests' expectations off a 30-row screen.
    if "twofiles" in line:
        # Long enough that the two files together overflow the pad, which is
        # what makes the scroll-across-a-boundary behaviour testable at all.
        big = "\n".join("y%02d" % i for i in range(30))
        emit({"type": "assistant",
              "message": {"content": [{"type": "tool_use", "id": "t3", "name": "Edit",
                                        "input": {"file_path": "util.rs",
                                                  "old_string": "x",
                                                  "new_string": big}}]}})
        emit({"type": "user",
              "message": {"content": [{"type": "tool_result", "tool_use_id": "t3",
                                        "content": "ok"}]}})
    emit({"type": "assistant",
          "message": {"content": [{"type": "text", "text": "Tool run finished."}]}})
    result_ok("Hello from fake.")
"#;

struct Harness {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    buf: Arc<Mutex<Vec<u8>>>,
    parser: vt100::Parser,
    consumed: usize,
    _home: tempdir::TempDir,
}

// Tiny tempdir without a dev-dependency: mktemp under the target dir.
mod tempdir {
    pub struct TempDir(std::path::PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "avim-pty-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl Harness {
    fn spawn() -> Self {
        let home = tempdir::TempDir::new("home");
        let fake = home.path().join("fake_claude.py");
        std::fs::write(&fake, FAKE_CLAUDE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_avim"));
        cmd.env("HOME", home.path());
        cmd.env("AVIM_CLAUDE_BIN", &fake);
        cmd.env_remove("TMUX"); // workspace key "default", sandboxed under HOME
        cmd.cwd(home.path());
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        // Leak the master so the PTY stays open for the test's lifetime.
        std::mem::forget(pair.master);

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let buf = buf.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = reader.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.lock().unwrap().extend_from_slice(&chunk[..n]);
                }
            });
        }

        Harness {
            child,
            writer,
            buf,
            parser: vt100::Parser::new(30, 100, 0),
            consumed: 0,
            _home: home,
        }
    }

    fn feed_new(&mut self) {
        let buf = self.buf.lock().unwrap();
        if buf.len() > self.consumed {
            self.parser.process(&buf[self.consumed..]);
            self.consumed = buf.len();
        }
    }

    fn screen_text(&mut self) -> String {
        self.feed_new();
        self.parser.screen().contents()
    }

    fn wait_for(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if self.screen_text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn wait_gone(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if !self.screen_text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn keys(&mut self, s: &str) {
        self.writer.write_all(s.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn assert_on_screen(&mut self, needle: &str, secs: u64) {
        if !self.wait_for(needle, secs) {
            panic!(
                "expected {needle:?} on screen within {secs}s.\n--- screen ---\n{}\n---",
                self.screen_text()
            );
        }
    }
}

#[test]
fn boots_composes_streams_and_quits_with_confirm() {
    let mut h = Harness::spawn();

    // Boot: header + sessions sidebar + welcome, already in Insert — just type.
    h.assert_on_screen("aeovim", 10);
    h.assert_on_screen("SPACES", 10);
    h.assert_on_screen("claude code underneath", 10);
    h.assert_on_screen("INSERT", 5);
    h.keys("hello fake agent");
    h.assert_on_screen("hello fake agent", 5);

    // Enter sends. The fake streams a reply; the full turn must land.
    h.keys("\r");
    h.assert_on_screen("Hello from fake.", 10);
    h.assert_on_screen("Bash(echo hi)", 10); // tool call rendered
    h.assert_on_screen("hi", 5); // its ⎿ result under it
    h.assert_on_screen("Tool run finished.", 10);

    // Send must STAY in Insert mode (the old drop-to-Normal made `q` lethal).
    assert!(
        h.screen_text().contains("INSERT"),
        "composer must stay live after send:\n{}",
        h.screen_text()
    );

    // The user prompt echoed into the transcript, and the conversation was
    // auto-named from it (header shows the slug).
    h.assert_on_screen("▎ you", 5);

    // Ctrl-C on an idle, empty composer → confirm overlay (NOT instant quit),
    // y → exit.
    h.keys("\x03");
    h.assert_on_screen("quit aeovim?", 5);
    h.keys("y");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = h.child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("avim did not exit after quit confirm");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn interrupt_stops_the_turn_and_the_session_survives() {
    let mut h = Harness::spawn();
    h.assert_on_screen("claude code underneath", 10);
    h.assert_on_screen("INSERT", 5);

    // Start the long streaming turn and interrupt it mid-stream (Ctrl-C with an
    // empty composer = interrupt while a turn is in flight).
    h.keys("slowturn\r");
    h.assert_on_screen("tick", 10);
    h.keys("\x03");
    h.assert_on_screen("⎋ interrupted", 10);

    // The same child must serve the next turn — long-lived session model.
    h.keys("still alive?\r");
    h.assert_on_screen("Hello from fake.", 10);
    // Wait for the WHOLE turn (tools included) — a Ctrl-C mid-turn would
    // interrupt instead of asking to quit.
    h.assert_on_screen("Tool run finished.", 10);

    // Cleanup: Ctrl-C (idle, empty composer) asks to quit; y confirms.
    h.keys("\x03");
    h.assert_on_screen("quit aeovim?", 5);
    h.keys("y");
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.child.try_wait().map(|o| o.is_none()).unwrap_or(false) {
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("avim did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn markdown_underscores_survive_and_slash_popup_is_safe_when_short() {
    let mut h = Harness::spawn();
    h.assert_on_screen("claude code underneath", 10);
    // snake_case in the composer must render literally (the old markdown pass
    // ate the underscores out of the composer echo too, via the same renderer).
    h.keys("check foo_bar_baz now");
    h.assert_on_screen("foo_bar_baz", 5);
    // typing "/" opens the slash popup — must not panic (it used to Clear an
    // unclamped rect; 30 rows is fine, but exercise the path end-to-end).
    h.keys("\x15"); // Ctrl-U clear
    h.keys("/cl");
    h.assert_on_screen("commands", 5);
    h.assert_on_screen("/clear", 5);
    // Esc drops to Normal (vim-style; draft kept) — the popup closes.
    h.keys("\x1b");
    let _ = h.wait_gone("/clear", 2);
    h.assert_on_screen("NORMAL", 5);
    // Ctrl-C in idle Normal asks to quit; y confirms.
    h.keys("\x03");
    h.assert_on_screen("quit aeovim?", 5);
    h.keys("y");
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.child.try_wait().map(|o| o.is_none()).unwrap_or(false) {
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("avim did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = h.wait_gone("quit", 1);
}

#[test]
fn sidebar_picker_and_board_drive_multiple_sessions() {
    let mut h = Harness::spawn();
    h.assert_on_screen("SPACES", 10);
    h.assert_on_screen("INSERT", 5);

    // Session 1: send a turn (auto-names the session off the prompt).
    h.keys("hello fake agent\r");
    h.assert_on_screen("Hello from fake.", 10);

    // Esc drops to Normal; Space n opens a fresh second session in Insert.
    h.keys("\x1b");
    h.assert_on_screen("NORMAL", 5);
    h.keys(" n");
    h.assert_on_screen("2 spaces", 5);
    h.assert_on_screen("claude code underneath", 5); // fresh welcome
    h.keys("tell me about ramen\r");
    h.assert_on_screen("Tool run finished.", 10);

    // ga fuzzy picker: "hello" matches only session 1; Enter focuses it.
    // (wait for NORMAL — a bare ESC byte followed instantly by 'g' would be
    // parsed as Alt-g by crossterm)
    h.keys("\x1b");
    h.assert_on_screen("NORMAL", 5);
    h.keys("ga");
    h.assert_on_screen("GO TO SPACE", 5);
    h.keys("hello");
    h.keys("\r");
    let _ = h.wait_gone("GO TO SPACE", 2);
    h.assert_on_screen("hello fake agent", 5);

    // Space t opens the task board (one row per session); Esc closes it.
    h.keys(" t");
    h.assert_on_screen("TASKS —", 5);
    h.keys("\x1b");
    assert!(
        h.wait_gone("TASKS —", 3),
        "board must close on Esc:\n{}",
        h.screen_text()
    );

    // `:2` jumps back to the second space.
    h.keys(":2\r");
    h.assert_on_screen("tell me about ramen", 5);

    // `:vs` splits the space into two chat panes (Insert in the new pane).
    h.keys(":vs\r");
    h.assert_on_screen("chat 2", 5);
    h.assert_on_screen("(2)", 5); // sidebar shows the pane count
    h.keys("\x1b");
    h.assert_on_screen("NORMAL", 5);
    // `:q` closes the focused pane immediately, leaving one chat. No prompt:
    // typing the command is the confirmation (Saxon, 2026-08-27).
    h.keys(":q\r");
    assert!(
        h.wait_gone("chat 2", 3),
        "pane must close on :q without asking\n{}",
        h.screen_text()
    );

    // `:diff` opens the diff pad over this chat's edit history.
    h.keys(":diff\r");
    h.assert_on_screen("DIFF", 5);
    h.assert_on_screen("demo.rs", 5);
    h.keys("\x1b");
    assert!(
        h.wait_gone("DIFF", 3),
        "diff pad must close on Esc\n{}",
        h.screen_text()
    );

    // Quit: Ctrl-c asks, y confirms; both children must die with us.
    h.keys("\x03");
    h.assert_on_screen("quit aeovim?", 5);
    h.keys("y");
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.child.try_wait().map(|o| o.is_none()).unwrap_or(false) {
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("avim did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The three things Saxon asked for on 2026-08-27: `:q` without a prompt,
/// a per-space directory reachable by fuzzy picker, and a diff pad that holds
/// every file changed in the session and renames its header as you scroll
/// from one file's hunk into the next.
#[test]
fn dir_picker_and_multi_file_diff_pad() {
    let mut h = Harness::spawn();
    h.assert_on_screen("INSERT", 5);
    h.keys("edit twofiles please\r");
    h.assert_on_screen("Tool run finished.", 10);
    h.keys("\x1b");
    h.assert_on_screen("NORMAL", 5);

    // `gd` opens the directory picker rooted at $HOME; Esc closes it.
    h.keys("gd");
    h.assert_on_screen("OPEN DIR", 5);
    h.keys("\x1b");
    assert!(
        h.wait_gone("OPEN DIR", 3),
        "dir picker must close on Esc\n{}",
        h.screen_text()
    );

    // `:pwd` reports the space's own directory. (Not asserting a `~` form:
    // the harness points HOME at a temp dir whose cwd canonicalizes to
    // /private/var/…, so the abbreviation correctly doesn't apply here.)
    // (The statusline elides a long path, so match its visible head.)
    h.keys(":pwd\r");
    h.assert_on_screen("/private/var/folders", 5);

    // `:cd` refuses a path that isn't a directory rather than inventing one.
    h.keys(":cd /nope/not/here\r");
    h.assert_on_screen("not a directory", 5);

    // The diff pad opens on the NEWEST file and knows it is one of two.
    h.keys(":diff\r");
    h.assert_on_screen("DIFF", 5);
    h.assert_on_screen("util.rs", 5);
    h.assert_on_screen("file 2/2", 5);

    // Ctrl-k jumps back a whole file — the header renames to the older one.
    h.keys("\x0b");
    h.assert_on_screen("demo.rs", 5);
    h.assert_on_screen("file 1/2", 5);

    // Scrolling down far enough crosses the boundary into util.rs, and the
    // header follows the row at the top of the viewport.
    for _ in 0..14 {
        h.keys("j");
    }
    h.assert_on_screen("util.rs", 5);
    h.assert_on_screen("file 2/2", 5);

    h.keys("\x1b");
    assert!(
        h.wait_gone("DIFF", 3),
        "diff pad must close on Esc\n{}",
        h.screen_text()
    );

    // `:q` on the last pane of the last space quits outright — no prompt.
    h.keys(":q\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.child.try_wait().map(|o| o.is_none()).unwrap_or(false) {
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("`:q` must quit without asking:\n{}", h.screen_text());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
