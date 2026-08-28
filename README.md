# aeovim

*vim, but the buffers are live coding agents and the operators drive them.*

**aeovim** is a standalone, keyboard-native Rust TUI for talking to coding agents — modal, vim-style, codex-simple: a **SPACES sidebar** on the left (a space holds 1–2 chats; two render as a thin-divider vsplit), one clean conversation surface, and vim's grammar over all of it. Launch it and you're typing (Insert); `Esc` is Normal; `:vs` splits; `ga` fuzzy-jumps; `:diff` reviews changes. The chrome stays quiet — cost, model detail, and permissions live behind `:cost` / `:status`, not on screen.

The project is **aeovim**; the command you run is **`avim`** (like Neovim → `nvim`).

It wraps the `claude` CLI (Claude Code) as one long-lived child over headless `stream-json`. It reuses Claude Code's own auth, tools, permissions, skills, and MCP servers — it doesn't re-implement any of that. Single-user, local macOS daily driver. Not distributed.

## Status

**Usable daily driver (2026-08 rebuild + multi-session redesign).** ~4,500 lines of Rust plus a PTY test harness. The 2026-08 rebuild replaced the one-child-per-turn skeleton with a long-lived session model and fixed the input/rendering/persistence defects; the redesign pass brought the sessions sidebar back on top of it, with true modal editing and a colour-agnostic token theme.

### What works today

- **Spaces, one surface:** the SPACES sidebar lists every space with a live status glyph (`●`/`○` running flash · `✗` error · `✓` idle). `Space ee` toggles it, `Space 1-0` / `gt` / `ga` (fuzzy picker) jump, `Space n` spawns, `d` deletes, `r` renames. Each chat is its own long-lived claude child; everything persists per tmux session and resumes via `--resume` (stale sessions self-heal).
- **Two chats per space:** `:vs` (or `Ctrl-w v`) adds a second chat pane — a thin-divider vsplit with slim `▎ chat N` headers. `Ctrl-h`/`Ctrl-l` (and `Ctrl-w`) walk sidebar ↔ pane ↔ pane; `:q` closes the focused pane, then the space; the composer always talks to the focused pane.
- **Diff pad, codediff-style:** `:diff` opens a full-screen review of the focused chat's edit history, modelled on [codediff.nvim](https://github.com/esmuellert/codediff.nvim) in its `inline` layout — a `Changes (N)` explorer down the left, the selected file's unified diff on the right. No `+`/`-` gutter signs: changed lines carry a background wash that runs to the end of the line, with a brighter tier punched through it on the words that actually differ. Deleted lines are unnumbered, the way codediff renders them as virtual rows. `]c`/`[c` step changes, `]f`/`[f` (or `Ctrl-j`/`Ctrl-k`) step files, `Tab` hides the explorer, `q` closes.
- **Modal, the vim way:** Insert on launch (Enter sends, stays Insert), `Esc` → Normal (scroll, jump, operate), `:` ex commands (`:q` `:vs` `:new` `:clear` `:diff` `:tasks` `:rename` `:cost` `:status` `:N`), `?` help. Mode pill in the statusline: NORMAL / INSERT / COMMAND / RENAME / CONFIRM.
- **Codex-simple chrome:** header is name + model; statusline is mode + name + a spinner only while the focused turn runs. No cost, ids, or permission flags on screen — `:cost` and `:status` answer those on demand as statusline toasts (truncated to 60 columns; no overlay yet).
- **Tasks:** `:tasks` / `Space t` — one row per chat (state · elapsed · last line); `⏎` focuses, `x` cancels that chat's turn.
- **Directories:** each space owns a working directory — `:cd <path>`, `:cd -`, bare `:cd` / `gd` for a fuzzy picker over `$HOME`, `:pwd` to check. Changing it restarts the space's children in the new cwd (refused mid-turn).
- **Colour-agnostic theme:** ten base tokens (lilac by default, straight from the author's nvim theme) with every panel/border/selection derived by mixing; override any token in `~/.config/aeovim/theme.toml` and the whole UI rethemes. The terminal background stays transparent.
- **Long-lived child:** one persistent `claude` process driven over stdin `--input-format stream-json`. Follow-up turns skip the session-reload cost entirely; the child survives across turns and interrupts.
- **Interrupt:** `Ctrl-C` (or `Esc` in Normal) interrupts the running turn via the control protocol (child stays alive; press again to force-kill). Quitting kills every child — nothing keeps editing files invisibly.
- **Clean surface:** open transcript with `▎ you` / `▎ claude` blocks, Claude-style `● Tool(...)` / `⎿ result` cards, colored +/- edit hunks, gutter-barred code blocks, bordered composer.
- **Streaming that scales:** settled messages are pre-wrapped into a render cache (keyed by revision/width), so a frame only clones the rows in view; token deltas are batched at ~30fps with input always serviced first. Scroll is sticky-bottom — content never yanks the viewport while you're reading; a `↕ %` tag shows when you're detached from the tail.
- **Tool output kept:** results correlated to their calls by `tool_use_id` (parallel calls render correctly), full text stored (head+tail capped), `Ctrl-t` expands/collapses.
- **Composer with a real cursor:** arrows/Home/End/Ctrl-a/e/w, grapheme-aware editing, display-width math (CJK/emoji safe), bracketed paste intact, sends queue up while a turn runs.
- **Persistence:** atomic writes, corrupt-file backup + report (never silent loss), pid lock so two instances can't clobber each other, old multi-space state files backed up before adoption.
- **Permissions:** dangerous by default (matches the author's `claude` alias); `--safe` switches to `--permission-mode acceptEdits`.
- **Tests:** 38 total — unit tests for wrapping/markdown/protocol/store/theme plus a PTY + VT-emulation harness that boots the real binary against a scripted fake `claude` (streaming, tool rendering, mid-stream interrupt with session survival, quit-confirm, and a spaces pass driving the sidebar, `ga` picker, `:vs` split, `:q` pane close, `:diff` pad, and tasks end-to-end).

### Designed, not yet built

- Orchestration beyond turn jobs (fan-out groups, loops, subagent rows on the board) — the board today lists real sessions/turns only.
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

Modal — Insert on launch, `Esc` is Normal. The authoritative, in-app reference is **`?`** / **`F1`** / **`/help`**; `avim --help` prints the current summary. The stable essentials:

| Key | Action |
|-----|--------|
| type + `Enter` | send (launches in Insert; stays there) |
| `Shift/Alt-Enter` · `Ctrl-j` | newline |
| `Esc` | Insert → Normal (draft kept) · in Normal: snap to the tail |
| `i` `a` `o` | Normal → Insert |
| `Ctrl-C` | interrupt the running turn (again = force-kill) · idle: quit (asks) |
| `Space ee` · `Ctrl-h`/`Ctrl-l` | toggle sidebar · walk sidebar ↔ panes |
| `Space 1-0` · `gt`/`gT` · `ga` | jump to space N · cycle · fuzzy picker |
| `Space n` · `Space t` | new space · tasks |
| `:vs` · `Ctrl-w v/h/l/w/q` | split into two chats · vim window commands |
| `:diff` | diff pad — `]c/[c` change · `]f/[f` file · `j/k` scroll · `Tab` panel |
| `:` | `:q` (pane→space) `:qa` `:new` `:clear` `:rename` `:tasks` `:cost` `:status` `:N` |
| `j`/`k` · `Ctrl-d`/`u` · `gg`/`G` | scroll (Normal) |
| `Ctrl-t` | expand / collapse tool output |
| `/clear` · `/rename` · `/mouse` · `/tools` · `/new` · `/tasks` | local slash commands |

## Docs

- [DESIGN.md](./DESIGN.md) — full design spec (UX model, architecture, orchestration, adapter seam, diff review).
- [IMPLEMENTATION_PLAN.md](./IMPLEMENTATION_PLAN.md) — milestone ladder and build plan.
- [INTEGRATION.md](./INTEGRATION.md) — Claude Code integration research: what the stream already emits, what to parse next, and the persistent-child milestone that unlocks in-TUI approval/interrupt.

<img width="1512" height="950" alt="image" src="https://github.com/user-attachments/assets/283b1478-1fd5-4673-8ab3-c4d0c5921605" />
