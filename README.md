# aeovim

*vim, but the buffers are live coding agents and the operators drive them.*

**aeovim** is a standalone, keyboard-native Rust TUI for talking to a coding agent. It applies the Neovim mental model — Insert to talk, Normal to read and steer — to one conversation with Claude Code, DeepSeek-TUI style: launch it and you're typing; everything on one clean surface. Multiplexing happens where it already lives — tmux; one `avim` per pane, each with its own persistent session.

The project is **aeovim**; the command you run is **`avim`** (like Neovim → `nvim`).

It wraps the `claude` CLI (Claude Code) as one long-lived child over headless `stream-json`. It reuses Claude Code's own auth, tools, permissions, skills, and MCP servers — it doesn't re-implement any of that. Single-user, local macOS daily driver. Not distributed.

## Status

**Usable daily driver (2026-08 rebuild, single-session).** ~4,000 lines of Rust plus a PTY test harness. The 2026-08 pass replaced the one-child-per-turn skeleton with a long-lived session model, fixed the input/rendering/persistence defects that made the skeleton unusable, then collapsed the experimental spaces/panes layer into one clean conversation per launch.

### What works today

- **One session per launch:** launches straight into Insert mode, ready to type. State (transcript, session id, title, cost) persists per tmux session; relaunch resumes the same claude conversation via `--resume`, and a session claude no longer knows self-heals into a fresh one.
- **Long-lived child:** one persistent `claude` process driven over stdin `--input-format stream-json`. Follow-up turns skip the session-reload cost entirely; the child survives across turns and interrupts.
- **Interrupt:** `Esc` / `Ctrl-C` interrupts the running turn via the control protocol (child stays alive; press again to force-kill). Quitting kills the child — nothing keeps editing files invisibly.
- **Clean surface:** header (title · model · permissions · session), open transcript with `❯ you` / `✦ claude` blocks, Claude-style `● Tool(...)` / `⎿ result` cards, colored +/- edit hunks, gutter-barred code blocks, lualine-style statusline, bordered composer. Lilac theme throughout.
- **Streaming that scales:** settled messages are pre-wrapped into a render cache (keyed by revision/width), so a frame only clones the rows in view; token deltas are batched at ~30fps with input always serviced first. Scroll is sticky-bottom — content never yanks the viewport while you're reading; a `↕ %` tag shows when you're detached from the tail.
- **Tool output kept:** results correlated to their calls by `tool_use_id` (parallel calls render correctly), full text stored (head+tail capped), `za` expands/collapses.
- **Composer with a real cursor:** arrows/Home/End/Ctrl-a/e/w, grapheme-aware editing, display-width math (CJK/emoji safe), bracketed paste intact, sends queue up while a turn runs.
- **Persistence:** atomic writes, corrupt-file backup + report (never silent loss), pid lock so two instances can't clobber each other, old multi-space state files backed up before adoption.
- **Permissions:** dangerous by default (matches the author's `claude` alias); `--safe` switches to `--permission-mode acceptEdits`.
- **Tests:** 34 total — unit tests for wrapping/markdown/protocol/store plus a PTY + VT-emulation harness that boots the real binary against a scripted fake `claude` (streaming, tool rendering, mid-stream interrupt with session survival, quit-confirm).

### Designed, not yet built

- Multi-agent orchestration (fan-out, task board, loops) — deliberately parked; tmux covers multiplexing for now.
- Vim-native **diff review**: `]c` / `[c` hunk motions, per-turn git approve/reject.
- Tree-sitter syntax highlighting (code rendering only).
- In-TUI permission approval + mid-turn steering over the control protocol (the session model supports it; the approval UI is the remaining piece).

## Install & run

```sh
cargo install --path .        # builds the `avim` binary into ~/.cargo/bin
avim                          # launch (dangerous permissions by default)
avim --safe                   # --permission-mode acceptEdits instead
avim --model <name>           # pick the Claude model
avim --help                   # flags + key reference
```

Sessions persist per tmux session; relaunch `avim` to resume where you left off.

## Keys

The keymap mirrors the author's Neovim config and is still moving. The authoritative, in-app reference is **`Space zz`** (cheatsheet); `avim --help` prints the current summary. The stable essentials:

| Key | Action |
|-----|--------|
| type + `Enter` | send (launches in Insert; stays there) |
| `Shift/Alt-Enter` · `Ctrl-j` | newline |
| `Esc` / `Ctrl-C` | interrupt the running turn (again = force-kill) |
| `Esc` (idle) | Normal mode: `j`/`k` scroll · `Ctrl-d/u` · `gg`/`G` |
| `za` / `zz` | expand tool output / jump to newest |
| `r` | rename conversation |
| `/clear` | wipe transcript, fresh session |
| `?` | cheatsheet · `q` quit (asks) · `:q` quits |

## Docs

- [DESIGN.md](./DESIGN.md) — full design spec (UX model, architecture, orchestration, adapter seam, diff review).
- [IMPLEMENTATION_PLAN.md](./IMPLEMENTATION_PLAN.md) — milestone ladder and build plan.
- [INTEGRATION.md](./INTEGRATION.md) — Claude Code integration research: what the stream already emits, what to parse next, and the persistent-child milestone that unlocks in-TUI approval/interrupt.

<img width="1512" height="950" alt="image" src="https://github.com/user-attachments/assets/283b1478-1fd5-4673-8ab3-c4d0c5921605" />
