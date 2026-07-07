# aeovim

*vim, but the buffers are live coding agents and the operators drive them.*

**aeovim** is a standalone, keyboard-native Rust TUI for multiplexing and orchestrating LLM coding agents. It applies the Neovim mental model — modes, motions, operators, buffers, tabs, splits — to conversations with coding agents, so spawning, steering, watching, and reviewing many agents at once is muscle memory rather than window juggling.

The project is **aeovim**; the command you run is **`avim`** (like Neovim -> `nvim`).

- v1 wraps the `claude` CLI (Claude Code) as long-lived child processes over headless `stream-json`.
- An orchestrator, not a viewer: parallel tasks, fan-out, skills, and avim-owned loops on a first-class job board.
- vim-native diff review with `]c` / `[c` hunk motions and per-turn git approve/reject.
- All backend detail sits behind an adapter seam so other models/CLIs (or a direct API) drop in later.

Single-user, local macOS daily driver. Not distributed.

## Docs

- [DESIGN.md](./DESIGN.md) — full design spec.
- [IMPLEMENTATION_PLAN.md](./IMPLEMENTATION_PLAN.md) — milestone ladder and build plan.

## Status

Pre-implementation. Design complete; the walking-skeleton spike is next (see the plan).
