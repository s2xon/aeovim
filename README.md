# aeovim

*vim, but the buffers are live coding agents and the operators drive them.*

**aeovim** is a standalone, keyboard-native Rust TUI for multiplexing and orchestrating LLM coding agents. It applies the Neovim mental model — modes, motions, buffers, tabs, splits — to conversations with coding agents, so spawning, steering, watching, and reviewing many agents at once is muscle memory rather than window juggling.

The project is **aeovim**; the command you run is **`avim`** (like Neovim → `nvim`).

v1 wraps the `claude` CLI (Claude Code) as child processes over headless `stream-json`. It reuses Claude Code's own auth, tools, permissions, skills, and MCP servers — it doesn't re-implement any of that. All backend detail sits behind an `AgentBackend` seam so other models/CLIs (or a direct API) can drop in later. Single-user, local macOS daily driver. Not distributed.

## Status

**Working walking skeleton — installable and in daily use.** ~3,200 lines of Rust across seven modules; builds, installs, and drives real multi-turn Claude Code sessions. This is well past the "pre-implementation" the earlier README claimed. The orchestration layer (fan-out, job board, diff review) is designed but not yet built — see the split below.

### What works today

- **Modal TUI** with a Space-leader keymap + which-key popup, ported from the author's Neovim config (nvim-tree / harpoon / bufferline / lualine / which-key). Lilac theme.
- **Two-level model:** a **Space** is a named container of 1–4 **Chats**. The sidebar lists Spaces; the active Space renders its Chats as split panes (single / vertical / horizontal / 2×2), focused pane bordered in bright purple.
- **Live Claude Code sessions:** each Chat spawns `claude` over `--output-format stream-json`; multi-turn continuity via `--session-id` then `--resume`.
- **Streaming transcript:** assistant messages, a thinking spinner, and Claude-style tool-call / tool-result rendering; slash-command popup; inline markdown; mode indicator; powerline status bar.
- **Navigation:** `Ctrl-hjkl` focus panes ↔ sidebar, `Tab` / `H` / `L` cycle chats, `Space 1-0` jump to a Space, sidebar add / rename / delete.
- **Space ops:** merge multiple Spaces (chats combined, ≤4), pop a chat into its own Space, split management.
- **Persistence:** Spaces (name + chats) saved per tmux session at `~/.local/state/aeovim/<session>.json`; relaunch resumes.
- **Inter-agent pipe:** a FIFO (`~/.local/state/aeovim/<key>.pipe`) lets one agent message another Space; a reader thread routes it into the target chat's transcript and the agent responds.
- **Permissions:** dangerous by default (`--dangerously-skip-permissions`); `--safe` switches to `--permission-mode acceptEdits`.

### Designed, not yet built

- Parallel fan-out of one prompt to N agents, each isolated in its own git worktree, as a first-class **job**.
- A quickfix-style **task board** with done / needs-input / error status.
- aeovim-owned **loop scheduler** and a **skills palette**.
- Vim-native **diff review**: `]c` / `[c` hunk motions, visual-select, per-turn git approve/reject on an apply baseline.
- Tree-sitter syntax highlighting (code rendering only).
- Persistent bidirectional child for in-TUI permission approval, interrupt (`Esc`), and mid-turn steering. (Today's one-child-per-turn model rules these out by design — see INTEGRATION.md.)

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
| `i` / `Esc` | compose / normal mode |
| `Enter` | send (in composer) |
| `Ctrl-h` / `Ctrl-l` | focus sidebar / chat panes |
| `H` / `L` / `Tab` | previous / next chat |
| `Space e` | toggle sidebar |
| `Space 1`–`0` | jump to Space N |
| `:` | command | 
| `Space zz` | cheatsheet · `q` quit |

## Docs

- [DESIGN.md](./DESIGN.md) — full design spec (UX model, architecture, orchestration, adapter seam, diff review).
- [IMPLEMENTATION_PLAN.md](./IMPLEMENTATION_PLAN.md) — milestone ladder and build plan.
- [INTEGRATION.md](./INTEGRATION.md) — Claude Code integration research: what the stream already emits, what to parse next, and the persistent-child milestone that unlocks in-TUI approval/interrupt.

<img width="1512" height="950" alt="image" src="https://github.com/user-attachments/assets/283b1478-1fd5-4673-8ab3-c4d0c5921605" />
