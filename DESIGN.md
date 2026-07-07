# aeovim — Design Document

*vim, but the buffers are live coding agents and the operators drive them.*

**Status:** Design spec for **aeovim**, a standalone Rust TUI. v1 wraps the `claude` CLI (Claude Code) as long-lived child processes over headless `stream-json`; single-user, local macOS daily driver; an **orchestrator** (parallel tasks, fan-out, skills, aeovim-owned loops) with a first-class job board and vim-native diff review; all Claude-specific wire detail sits behind an `AgentBackend` adapter seam so future models/CLIs (or a direct API) drop in without touching the UI. Claims about the `claude` CLI are marked **verified** (probed live against v2.1.201) or **verify:** (inferred, must be confirmed in the walking-skeleton spike).

---

## Table of Contents

1. [Overview, Goals & Non-Goals](#1-overview-goals--non-goals)
2. [UX Model & Keybinding Specification](#2-ux-model--keybinding-specification)
3. [System Architecture](#3-system-architecture)
4. [Orchestration: Parallel Tasks, Fan-out, Skills & Loops](#4-orchestration-parallel-tasks-fan-out-skills--loops)
5. [Agent Abstraction & the Claude CLI Adapter](#5-agent-abstraction--the-claude-cli-adapter)
6. [Diff Review & Edit-Approval Model](#6-diff-review--edit-approval-model)
7. [Tree-sitter Integration](#7-tree-sitter-integration)
8. [State, Persistence & Configuration](#8-state-persistence--configuration)
9. [Open Questions](#9-open-questions)

---

## 1. Overview, Goals & Non-Goals

### 1.1 What aeovim is

**aeovim** ("agent vim") is a standalone, keyboard-native TUI — its own Rust binary you launch (`avim`) that drops you into a full-screen modal interface — for **multiplexing and orchestrating LLM coding agents**. It applies the Neovim mental model (modes, motions, operators, buffers, tabs, splits, quickfix) to conversations with coding agents so that spawning, steering, watching, and reviewing many agents at once is muscle memory rather than window juggling.

For v1 the only backend is the existing `claude` CLI (Claude Code), wrapped as a long-lived child process in headless `stream-json` mode. aeovim reuses Claude Code's own auth, tools, permissions, skills, subagents, MCP servers, and file editing — it does **not** re-implement any of that. Its job is to make those capabilities *keyboard-native and multiplexed*, and to add the thin layer nobody else has: a modal grammar over agents, a first-class job/task board, and vim-style diff review.

### 1.2 The problem

Driving one coding agent is easy. Driving *many* is where current tooling falls down:

| Pain | What it looks like today | What aeovim does |
|------|--------------------------|----------------|
| **Context-switching across agents** | Cmd-tab between terminals / tmux prefix dances; you lose track of which agent is doing what | `gt`/`gT` cycle agents, `{count}gt` jumps, `ga` fuzzy-picks — no mouse, no prefix key |
| **Parallelism is manual** | You hand-spawn N `claude -p` processes, each in its own pane, and eyeball them | Fan-out one prompt to N agents (each in its own git worktree) as a single first-class **job**, with a one-key "adopt this one" |
| **"Which agent needs me?"** | Nothing surfaces a blocked/finished agent; you poll by cycling panes | A quickfix-style **Task Board** with green/yellow/red status (done / needs-input / error) |
| **Diff review is scroll-and-squint** | Agents apply edits directly; you read a wall of colored diff and hope | Vim hunk motions `]c`/`[c`, visual-select hunks, `s` approve / `x` reject on a per-turn git baseline |
| **Recurring / babysit work** | You re-type the same prompt, or `/loop` dies when the headless process exits | aeovim owns the loop scheduler — pausable, cancellable, cost-capped, on the board |
| **Skills / slash-commands / subagents** | Buried in one interactive session | Surfaced from `system/init` as a palette, invokable on any focused agent |

aeovim is an **orchestrator, not a viewer**: from inside it you can do everything Claude Code can do — skills, slash-commands, loops, subagents, MCP tools, background jobs — driven entirely by the keyboard.

### 1.3 Goals

1. **Everything Claude Code can do, keyboard-driven and multiplexed.** Spawn, focus, prompt, interrupt, resume, and close agents with vim motions and `:`-commands. Invoke skills/slash-commands, run loops, fan out, inspect subagents, and **approve gated tool calls** without leaving aeovim.
2. **True modal editing over conversations.** Normal = navigate & command; Insert = compose a prompt; Visual = select (message ranges, diff hunks); Command-line = ex commands. Nothing reaches an agent until you send it.
3. **Parallelism as a first-class object.** N concurrent agents plus background loops and fan-out groups, each a **job** with state, cost, and a status glyph on a shared board.
4. **vim-native codediff review.** Per-turn git diffs navigated with `]c`/`[c`, hunk-level approve/reject via git.
5. **Non-blocking, streaming UI.** Many child processes feed *one* render loop; token deltas update unfocused buffers without stalling the focused one.
6. **A clean adapter seam.** All Claude-specific wire details sit behind an `AgentBackend`/`SessionHandle` trait so a Codex CLI or direct-API backend is a new impl, not a refactor — and so `:model` can switch models within a backend.
7. **A daily driver for one author.** Configurable enough to live in (TOML keymaps, theme, defaults); clean, but with no stranger-facing packaging polish.

### 1.4 Non-goals (v1)

| Non-goal | Why |
|----------|-----|
| **Not a Neovim plugin / nvim embed.** | aeovim is its own binary with its own event loop. The vim model is *emulated*, not hosted. |
| **No direct Anthropic (or any) API in v1.** | v1 wraps the `claude` CLI. All auth, tools, permissions, skills, MCP, and file editing come from Claude Code. Direct-API is a *future backend* behind the adapter seam. |
| **Not distributed / not multi-user.** | Single author, local macOS. No install wizard, no cross-machine sync, no stranger-facing docs. |
| **Not a general text editor.** | Buffers are agent sessions, not files. The operator set is deliberately tiny (yank, diff-ops). |
| **No per-*source-line* pre-apply approval in v1.** | Claude gates whole tool calls, not hunks. v1 reviews edits *after* they apply, via git hunks. A coarse pre-apply gate is a defined fast-follow (§6.2). |
| **No reimplementation of Claude's capabilities.** | Skills, subagents, MCP, context management, background agents are **inherited by wrapping the CLI**, not rebuilt. |
| **tree-sitter is not the tab manager.** | tree-sitter's real role is code-block highlighting and optional in-buffer structural motions. Tab/agent management is aeovim's own object model. |
| **Arbitrary N-way window trees are deferred.** | v1 ships tabs + a two-pane vsplit (transcript‖diff, or transcript‖another-agent). General N-way splits are a fast-follow. |

### 1.5 The vim → aeovim mental model

The core design move is a precise mapping from vim/Neovim objects to agent-orchestration objects. **v1 collapses to one model to keep muscle memory honest: an agent *is* a tab.** `gt`/`gT` cycle agents. Splits are additional *viewports within a tab* — the focused agent's diff, or a second agent's transcript for side-by-side. The general "tab = arbitrary window tree" abstraction is deferred until N-way layouts land; when it does, layout-tabs get their own bind and `gt` stays on agents.

| vim concept | aeovim meaning (v1) | Notes |
|-------------|-------------------|-------|
| **buffer** | one agent conversation / session | Owns `session_id`, backend, model, permission mode, cwd, worktree, transcript, streaming message, capability palette, status, cost. |
| **tab** | one **agent** (its home layout) | Shown in the tabline; `gt`/`gT`/`{count}gt` cycle agents. |
| **window** | a viewport onto a buffer | Own scroll + cursor. A window can view *any* agent, so a split may show the focused agent's diff **or another agent's transcript**. |
| **split / vsplit** | two views side by side | `<C-w>` navigation; v1 wires transcript‖diff and transcript‖other-agent. |
| **Normal mode** | navigate & command | Motions between agents, jump to messages, navigate diffs, spawn/fan-out/loop. **No text reaches the agent.** |
| **Insert mode** | compose a prompt to the focused agent | A `tui-textarea` composer; multiline is natural; an explicit *Send* submits the turn. |
| **Visual mode** | select ranges | `V` selects whole messages; in a diff, visual selects hunks; operators act on the selection. |
| **Command-line (`:`)** | ex commands | `:new`, `:fanout`, `:loop`, `:model`, `:perm`, `:tasks`, `:q`… (`/` searches the focused buffer). |
| **motions / operators** | move between agents, jump to messages, navigate diffs | `gt`/`gT`/`{count}gt`, `ga`, `{`/`}`, `]c`/`[c`. Operator set is intentionally small. |
| **quickfix / `:copen`** | the **Task Board** | A dedicated buffer listing all jobs; `<Enter>` on a row focuses that agent. |
| **`:q` / `:qa`** | detach focused agent (keeps it running) / quit aeovim | `:q` hides+detaches; `:bd!`/`<leader>K` kills+reaps (§2, §3.4). |
| **a "job / task"** | a running unit of agent work | The backend-agnostic unit — see below. |

#### The job/task concept

A **job** is the atomic, backend-agnostic unit of agent work that has state and can be watched, jumped-to, and cancelled from the board:

| Job kind | What it is | Ownership |
|----------|-----------|-----------|
| `turn` | one in-flight user turn (prompt → `result`) | inherited (the CLI runs it) |
| `fanout` | one prompt sent to N sessions, each in its own worktree, aggregated + harvestable | **aeovim-owned** group |
| `loop` | a prompt re-issued on an interval / self-paced — pausable, cost-capped | **aeovim-owned** scheduler (not claude's `/loop`) |
| `subagent` | Task-tool work, detected via `parent_tool_use_id`, nested under its parent | inherited (surfaced) |
| `background` | a detached `claude --bg` session, seeded from `claude agents --json` | inherited (wrapped, fast-follow) |

Every job carries `{ id, kind, agent, status: green|yellow|red, elapsed, cost, last_line }`. Status is a single attention model — green = done, yellow = needs input, red = error — so a blocked or finished agent surfaces on the board without you cycling tabs.

```rust
// The shape the whole app orbits (illustrative, not final):
enum JobKind { Turn, Fanout, Loop, Subagent, Background }
enum JobStatus { Running, NeedsInput, Done, Error } // -> green/yellow/red glyphs

struct Job {
    id: JobId,
    kind: JobKind,
    agent: AgentId,        // which session/buffer it belongs to
    status: JobStatus,
    cost_usd: f64,         // summed from each turn's result.total_cost_usd
    last_line: String,     // most recent event summary for the board
}
```

#### Wireframe: the aeovim surface

```
┌ tabline ───────────────────────────────────────────────────────────────────┐
│ 1:refactor-api●  2:write-tests◑  3:fanout(4)◐  4:loop:watch-ci○   [+]        │
├ main region (focused agent, or transcript ▏ diff/other-agent vsplit) ────────┤
│ transcript                        ▏ working-tree diff (per-turn baseline)    │
│  you  ▸ refactor the auth module  ▏  @@ -12,7 +12,9 @@ fn login(...)          │
│  ai   ▸ I'll update src/auth.rs…  ▏  - let t = mint(u);      [c]             │
│       ▸ [Edit src/auth.rs]        ▏  + let t = mint(&u)?;   >]c< s:approve    │
│  ▍streaming…                      ▏  + audit(&t);            x:reject         │
├ Task Board (overlay, toggle <leader>t) ──────────────────────────────────────┤
│  id  kind      agent          status  elapsed  cost    last                  │
│  01  turn      refactor-api   ● run   0:12     $0.03   editing src/auth.rs    │
│  02  fanout×4  write-tests    ◑ wait  1:40     $0.21   2/4 need review        │
│  03  loop      watch-ci       ○ idle  —        $0.08   next run in 4m         │
├ statusline ──────────────────────────────────────────────────────────────────┤
│ NORMAL  refactor-api  sonnet  acceptEdits  ⌁ 3.1k tok  $0.03                  │
└ :fanout 4 add doctests to the public API_ ───────────────────────────────────┘
```

The rest of this document specifies how each piece realizes this mental model concretely.

---

## 2. UX Model & Keybinding Specification

This section defines the interaction contract: the modes, the on-screen surfaces, and the complete keymap. The target is *no new muscle memory* — a Neovim user should guess most bindings. Every user-named feature (agent-tab motion, window nav, g-jumps, easy close, compose/send, history scroll, `]c`/`[c`, apply/reject, visual select, spawn/fanout/loop/skill-palette/board) has an explicit home.

### 2.1 The vim → aeovim object model

aeovim keeps vim's noun/verb grammar but rebinds the nouns to conversations and jobs (the object model in §1.5). tree-sitter is used only for code highlighting and optional in-buffer structural motions — **not** for managing tabs (the object model is aeovim's own, held in slotmaps).

### 2.2 Modes

Five modes, mirroring vim, plus a transient Operator-Pending state. Mode is always shown in the statusline.

| Mode | What it does | Enter from | Leave to |
|---|---|---|---|
| **Normal** | Navigate transcript & agents, run motions, review diffs, spawn/fanout/loop, open palettes and the board. **No keystroke reaches the agent.** | default; `Esc` from any mode | any |
| **Insert** | Edit the focused agent's **prompt composer** (`tui-textarea`). `Enter` inserts a newline; an explicit **Send** submits the turn. | `i` `a` `o` `A` `I` `cc` | `Esc` → Normal |
| **Visual** | Select ranges. `v` char, `V` line = whole messages, `Ctrl-v` block. In a diff, visual selects a **run of hunks**. | `v` `V` `Ctrl-v` | `Esc` / operator |
| **Command-line** | `:` ex-commands (§2.5); `/` `?` search within the focused buffer. | `:` `/` `?` | `Enter`/`Esc` |
| **Operator-Pending** | Transient between an operator key and its motion. | after an operator key | on motion / timeout |

```
                 i a o A I cc                :  /  ?
   ┌──────────┐ ───────────▶ ┌────────┐   ┌──────────┐
   │  NORMAL  │               │ INSERT │   │ COMMAND  │
   │          │ ◀─────────── │        │◀─▶│  / search│
   └────┬─────┘     Esc       └────────┘   └──────────┘
        │  v V C-v                 ▲  Esc
        ▼                          │
   ┌──────────┐   operator     ┌───┴────────┐
   │  VISUAL  │───────────────▶│ OP-PENDING │  (count+op+motion)
   └──────────┘   Esc / y s x  └────────────┘   timeoutlen resets
```

### 2.3 Screen anatomy & wireframes

Four stacked regions: **tabline**, **main region** (focused agent, or a two-pane vsplit, with the board as an overlay), **statusline**, and a **command row** (Command-line mode only). Tab glyphs: `⟳` streaming, `●` needs-input (yellow), `◍` loop/background running, `✓` idle/done (green), `✗` error (red).

**Full screen — single conversation focused**

```
┌ 1:refactor-api ⟳   2:write-tests ●   3:api-docs ◍   4:review ✓        [+] ┐
│                                                                            │
│  ▎you  128k ctx                                        14:02              │
│  ▎ refactor the parser to return Result instead of panicking             │
│                                                                            │
│  ▎claude · sonnet                                      14:02              │
│  ▎ I'll change `parse_expr` to propagate errors. Editing 3 files…         │
│  ▎   ⚙ Edit  src/parser.rs   (+18 −6)                                      │
│  ▎ ```rust                                                                 │
│  ▎ pub fn parse_expr(t: &mut Lexer) -> Result<Expr, ParseError> {    ◀ ts │
│  ▎     let lhs = parse_atom(t)?;                                           │
│  ▎ ```                                                                     │
│  ▎ Done — 2 files changed, tests still green.  ]c to review the diff.     │
│ ┌ compose (i to edit) ────────────────────────────────────────────────┐   │
│ │ also add a unit test for the empty-input case_                       │   │
│ └──────────────────────────────────────────────────────────────────────┘  │
├────────────────────────────────────────────────────────────────────────────
│ NORMAL   refactor-api   sonnet·acceptEdits   3⟳ jobs   $0.42   msg 6/6      │
└────────────────────────────────────────────────────────────────────────────
```

**Transcript ‖ diff vsplit** (`:vsplit` / `:diff`; `Ctrl-w l` to cross into the diff, `]c`/`[c` to walk hunks)

```
┌ 1:refactor-api ⟳ ───────────────────────┬ DIFF  src/parser.rs  hunk 2/5 ──┐
│ ▎claude · sonnet                         │ @@ -41,6 +41,9 @@ parse_expr     │
│ ▎ Editing src/parser.rs…                 │  fn parse_expr(t) {              │
│ ▎  ⚙ Edit src/parser.rs (+18 −6)         │-    let lhs = parse_atom(t);     │
│ ▎ Done. ]c to review.                    │-    if lhs.is_none() { panic!() }│
│ ▎                                        │+    let lhs = parse_atom(t)?;  ◀│ ← cursor / hunk
│ ▎                                        │ @@ -58,3 +61,7 @@                 │
│ ┌ compose (i) ─────────────────────────┐ │+    Ok(Expr::Binary(lhs, rhs))   │
│ │ _                                    │ │                                  │
│ └──────────────────────────────────────┘ │  s approve · x reject · S/X all  │
├──────────────────────────────────────────┴──────────────────────────────────
│ NORMAL   refactor-api   sonnet·acceptEdits   3⟳   $0.42   ]c hunk 2/5 (+27)  │
└──────────────────────────────────────────────────────────────────────────────
```

**Transcript ‖ other-agent transcript** — the "multiple conversations on screen" case: `:vsplit 2` shows agent 2's transcript beside the focused one; `<C-w>` crosses between them; each pane scrolls independently.

**Task Board** (`<leader>t` / `:tasks` — overlay; `<Enter>` focuses the agent, `dd` cancels)

```
┌ TASK BOARD ── 3 running · 1 blocked · 5 total ──────────────────── :tasks ┐
│  id    kind      agent          state         elapsed  cost    last line   │
│ ▎j01   turn      refactor-api   ⟳ running       0:12   $0.42   editing par… │
│  j02   fanout▸   write-tests    ● needs-input   1:04   $0.31   Bash denied… │
│    ├ j02.a  member  worktree/a  ⟳ running       1:04   $0.14   adding case… │
│    └ j02.b  member  worktree/b  ✓ done          0:58   $0.17   3 tests pass │
│  j03   loop⟳     poll-ci        ◍ sleeping 4m  12:00   $0.90   run #7 green │
│  j04   subagent  refactor-api   ✓ done          0:20   $0.05   (Task: grep) │
│  j05   background docs-index     ✗ error         0:03   $0.01   rate limit   │
├────────────────────────────────────────────────────────────────────────────
│  ⏎ focus · a approve+retry · dd cancel · r restart · p pause · gm adopt/gd drop │
└────────────────────────────────────────────────────────────────────────────
```

**Skill / slash-command palette** (`<leader>s` / `:skills` — populated live from `system/init`)

```
┌ SKILLS & COMMANDS  (focused: refactor-api) ───────────── fuzzy: cod_ ─────┐
│ > /code-review          review the current diff                           │
│   /deep-research        fan-out web research                              │
│   /verify               run app & confirm behavior                        │
│   Skill: two-pass-edit  (.claude/skills/…)                               │
├────────────────────────────────────────────────────────────────────────────
│  ⏎ send as turn · type free-text args · Esc cancel                          │
└────────────────────────────────────────────────────────────────────────────
```

The palette fuzzy-matches **names** and lets you type free-text args. It does not claim structured arg completion — `system/init` enumerates command/skill *names*, not argument schemas. A later pass may parse `.claude/skills/*/SKILL.md` / command frontmatter for arg hints.

**Agent picker** (`ga` — g-jump to a conversation by fuzzy name/number)

```
┌ GO TO AGENT ─────────────────────────────────── fuzzy: test_ ───┐
│ > 2  write-tests    ● needs-input   src/…/tests   sonnet         │
│   1  refactor-api   ⟳ running       src/parser    sonnet         │
│   3  api-docs       ◍ loop          docs/         opus           │
└──────────────────────────────────────────────────────────────────┘
```

### 2.4 Keymaps

Leader is `Space`. All multi-key sequences resolve through the keymap trie (§2.6), so `gt`, `]c`, and `<C-w>l` are ordinary trie paths, not special cases.

**Normal — scroll & transcript navigation**

| Key | Action |
|---|---|
| `j` / `k` | Down / up one line |
| `<C-e>` / `<C-y>` | Scroll transcript one line (keep cursor) |
| `<C-d>` / `<C-u>` | Half-page down / up |
| `<C-f>` / `<C-b>` | Page down / up |
| `gg` / `G` | Top / bottom of transcript |
| `}` / `{` | Next / previous **message** |
| `]t` / `[t` | Next / previous **tool call** |
| `zt` / `zz` / `zb` | Reposition current line top / center / bottom |
| `n` / `N` | Next / prev search match |

**Normal — agent tabs & windows**

| Key | Action |
|---|---|
| `gt` / `gT` | Next / previous agent |
| `{count}gt` | Go to agent *count* (e.g. `3gt`) |
| `ga` | **g-jump**: fuzzy agent picker |
| `<C-w>h/j/k/l` | Move focus between windows (transcript ↔ diff ↔ other agent) |
| `<C-w>v` / `<C-w>s` | vsplit / split (v1: two-pane vsplit wired; N-way fast-follow) |
| `<C-w>c` / `<C-w>q` | Close focused window (view only, agent keeps running) |
| `<C-w>o` | Only — close other windows |
| `<C-w>=` | Equalize window sizes |

**Normal — compose & send**

| Key | Action |
|---|---|
| `i` / `a` | Insert into composer (before / append at end) |
| `o` | New prompt line, enter Insert |
| `A` / `I` | Append at line end / insert at first non-blank |
| `cc` | Clear composer and enter Insert |
| `<C-Enter>` (kitty) or `<leader><Enter>` | **Send** the composed prompt as a turn |
| `<C-c>` | **Interrupt** the current turn (control-protocol `interrupt` when available, else `killpg`+resume) |
| `.` | Resend last prompt to focused agent |

**Send-key note (terminal-dependent, must-verify at startup):** many terminals cannot distinguish `Ctrl+Enter` from `Enter` without the kitty keyboard protocol / CSI-u. aeovim probes enhanced-key support at startup and enables it (Ghostty supports it). If unavailable, aeovim falls back to a guaranteed single-keystroke Insert-mode send configured in `keymap.toml` (default: `Alt+Enter` sends, `Enter` newlines). A single-keystroke send from Insert on the author's actual terminal is a hard requirement — never force an `Esc`→`<leader><Enter>` mode round-trip on the most frequent action.

**Normal — diff / codediff review** (per-turn git baseline; §6)

| Key | Action |
|---|---|
| `]c` / `[c` | Next / previous **hunk** |
| `]C` / `[C` | Last / first hunk |
| `s` | **Approve** hunk under cursor (aeovim-side keep-mark) |
| `x` | **Reject** hunk (reverse-apply via `git apply -R` / `git restore`) |
| `S` / `X` | Approve all / reject all hunks in file |
| `u` | Undo last hunk stage/reject |
| `dv` | Toggle inline word-level diff for the hunk |
| `<Enter>` (on a tool-call line) | Open that file's diff in the vsplit |

*(`s`/`x` are aeovim's own scheme, not a claim of gitsigns/fugitive fidelity — those tools use `s`/`u` and `<leader>hs`/`<leader>hr` respectively. `s`/`x` are unused in transcript context so there's no collision.)*

**Normal — orchestration** (leader-prefixed)

| Key | Action |
|---|---|
| `<leader>n` | **Spawn** a new agent/task (`:new`) |
| `<leader>f` | **Fan-out** the composed prompt to N agents |
| `<leader>l` | **Start a loop** on the focused agent |
| `<leader>L` | **Stop** the focused agent's loop |
| `<leader>s` | Skill / slash-command palette |
| `<leader>t` | Toggle the Task Board |
| `<leader>m` | Change model (`:model`) |
| `<leader>p` | Cycle permission mode (`:perm`) |
| `<leader>c` | Cancel/interrupt the focused Job |
| `<leader>q` | **Detach** (hide) the focused agent; it keeps running on the board |
| `<leader>K` | **Kill+reap** the focused agent (`:bd!`) |

*(Bare `q` is intentionally left unbound — in vim `q` is macro-record, a core reflex; binding it to a destructive close would surprise a vim native. Close/detach is `:q`/`<leader>q`; kill is `<leader>K`.)*

**Visual mode**

| Key | Action |
|---|---|
| `v` / `V` / `<C-v>` | Char / line (whole messages) / block select |
| `]c` / `[c` | Extend selection to next / prev hunk |
| `y` | Yank selection to clipboard |
| `s` / `x` | Approve / reject **all selected hunks** |
| `:` | Start an ex range command `:'<,'>` |
| `Esc` | Leave Visual |

**Insert mode** (mostly passthrough to `tui-textarea`)

| Key | Action |
|---|---|
| printable / `Enter` | Insert text / newline into composer |
| send-key (kitty `<C-Enter>` or fallback) | Send the turn without leaving Insert |
| `<C-w>` / `<C-u>` | Delete word / to line start |
| `<C-r>{reg}` | Paste register |
| `Esc` | Return to Normal |

**Command-line & search**

| Key | Action |
|---|---|
| `:` | Ex command line (§2.5) |
| `/` / `?` | Search forward / back in focused buffer |
| `<Tab>` | Complete command / agent id / skill name |
| `<C-p>` / `<C-n>` | Previous / next command history |

### 2.5 Command-line commands

Each maps to a verified CLI capability or an aeovim-owned object.

| Command | Effect |
|---|---|
| `:new [model] [--cwd path] [--worktree]` | Spawn a new agent; aeovim mints the `--session-id` UUID. |
| `:q` / `:q!` | **Detach** the focused agent (view closes, agent keeps running on the board). `!` skips the confirm. |
| `:bd` / `:bd!` | Kill + reap the focused agent's child group. |
| `:qa` / `:qa!` | Quit aeovim (reaps all child process groups). |
| `:b {id}` / `:agent {id}` | Focus an agent by id/name. |
| `:ls` / `:agents` | List **aeovim-owned** sessions. (Merging `claude agents --json` is gated behind a version check and deferred to the background-agents fast-follow — its JSON shape is version-fragile; see §4.6.) |
| `:model {fable\|opus\|sonnet\|full-id}` | Change the focused agent's model (§5.6; warns about context-reload cost when a restart is required). |
| `:effort {low\|medium\|high\|xhigh\|max}` | Set reasoning effort. |
| `:perm {acceptEdits\|default\|plan\|bypassPermissions}` | Set permission mode. *(verify: `manual` is reportedly an alias of `default`; `:perm` lists the canonical name only after confirming against `claude --help` on the pinned version. `auto` included only if supported.)* |
| `:fanout {N} {prompt}` / `:fanout {id,…} {prompt}` | Dispatch one prompt to N new agents (each in its own worktree) or an explicit set. |
| `:loop [interval] {prompt\|/cmd}` / `:loop stop` | Start/stop an **aeovim-owned** loop scheduler (§4.5). |
| `:skill {name} [args]` / `:cmd /{slash}` | Invoke a skill/slash-command on the focused agent (slash-prefixed user turn). |
| `:skills` | Open the skill palette. |
| `:tasks` / `:board` | Open the Task Board. |
| `:vsplit [id]` / `:split [id]` | Open a vsplit/split; optionally view another agent; default vsplit is transcript‖diff. |
| `:diff` | Open the focused agent's per-turn diff in the vsplit. |
| `:only` | Close all but the focused window. |
| `:resume {session_id}` | Cold-attach an existing session. |
| `:interrupt` | Cancel the current turn. |
| `:messages` | Dump the raw normalized event log (debug). |

### 2.6 Input engine (Rust sketch)

Keys are resolved per-mode by a trie that accumulates `{count}{operator}{count}{motion}`, like Neovim's `do_pending` path. `gt`/`gT`/`{count}gt`, `<C-w>l`, `ga`, and `]c`/`[c` need no special-casing. A bare leading `0` is the column-0 motion, not a count.

```rust
enum Mode { Normal, Insert, Visual(VisualKind), OperatorPending, Command }
enum VisualKind { Char, Line, Block }

struct PendingInput {
    count1: Option<u32>,          // 3gt -> GotoTab(3)
    operator: Option<Operator>,   // tiny set: Yank, ApproveHunk, RejectHunk
    count2: Option<u32>,
    chord: Vec<KeyEvent>,         // walked against the trie
}

enum Action {
    NextMessage, PrevMessage, Scroll(ScrollAmt), Goto(GotoTarget),
    NextTab, PrevTab, GotoTab(u32), AgentPicker, Win(Dir), Split(SplitDir),
    DetachAgent, KillAgent,
    NextHunk, PrevHunk, ApproveHunk, RejectHunk, UndoHunk,
    EnterInsert(InsertAt), EnterVisual(VisualKind), EnterCommand,
    SendPrompt, Interrupt,
    SpawnAgent, Fanout, StartLoop, StopLoop, SkillPalette, TaskBoard,
    ChangeModel, CyclePerm, CancelJob,
    Ex(String),
}

enum Resolve { Pending, Dispatch(Action, u32 /*count*/), NoMatch }
struct TrieNode { edges: HashMap<KeyEvent, TrieNode>, leaf: Option<Action> }
```

Resolution per `KeyEvent` in Normal: **(1)** leading `1..=9` then `0..=9` accumulate into `count1` (`0` with no pending count = col-0 motion); **(2)** an operator key sets `operator`, switches to `OperatorPending`, `count2` may accumulate, then a motion (or doubled op) completes it; **(3)** otherwise walk the trie — leaf with no out-edges → `Dispatch(action, count1*count2)`; ambiguous prefix → `Pending` + arm a `timeoutlen` timer (~1000 ms); no edge → `NoMatch`, bell, reset; **(4)** on timeout while `Pending`, dispatch the shorter complete prefix if one exists, else reset.

`update()` consumes the `Action` and returns effects that run off the UI task, so key handling never blocks streaming. Insert mode uses a near-passthrough `InsertMap` (only `Esc`, the send-key, and editing chords intercepted); Command mode uses a `CommandMap`. The resolver is pure `KeyEvent → Resolve` and unit-testable.

### 2.7 Notes on leader, timeouts, and defaults

- **Leader = `Space`**, `timeoutlen = 1000 ms`, both overridable in `config.toml` (`crokey` parses vim-style bind strings).
- **Statusline contract**: always renders `MODE`, focused agent name, `model·permission_mode`, the **running-Job count** (`3⟳`), accumulated cost, and a context indicator (`msg 6/6`, or `]c hunk 2/5 (+27)` in a diff).
- **Config-first**: every binding and `:`-command name is data in the config; the tables above are shipped defaults.

---

## 3. System Architecture

aeovim is a single Rust binary structured as **The Elm Architecture over a tokio multi-thread runtime**: one owner task holds all mutable state, a pure `update` reducer mutates it, and every side effect runs off the UI task and reports back as an event. Nothing that touches the terminal blocks on IO; nothing that does IO touches the terminal.

Three invariants make streaming N agents into one screen tractable:

1. **Single writer.** Only the UI task mutates `App`. No `Arc<Mutex<App>>`, no lock contention on the hot render path. Everything else communicates by message.
2. **Reader-side coalescing over an unbounded UI channel.** Per-child reader tasks continuously drain their pipe and collapse high-frequency token deltas *in place* before forwarding, so a flood of `content_block_delta` lines can never starve the render loop or block the child (§3.3).
3. **Adapter seam.** The UI, input engine, and orchestrator only ever see *normalized* `AgentEvent`s. `claude`'s stream-json wire format lives entirely behind `ClaudeCodeBackend`.

### 3.1 Component diagram

```
                                   ┌───────────────────────────────────────────────┐
                                   │             main + tokio runtime              │
                                   │  resolve claude abs-path · install alt-screen  │
                                   │  + raw mode (panic hook + Drop guard) · SIGINT  │
                                   │  group-reaper · spawn UiTask                    │
                                   └───────────────────────┬───────────────────────┘
                                                           │ owns
   crossterm EventStream ───────────┐                      ▼
   40fps tick ──────────────────────┤        ┌─────────────────────────────┐
   shutdown token ──────────────────┼───►     │           UiTask            │  single writer of App
                                    │        │  select! { events, rx,      │
   mpsc<AppEvent> (unbounded) ──────┘        │            tick, shutdown } │
        ▲                                    │  KeymapEngine → Action      │
        │ AppEvent                           │  update(App, msg) → Effects │
        │                                    │  if dirty { Renderer.draw } │
        │                                    └───────┬─────────────┬───────┘
        │                                    Effects │             │ &App (read-only)
        │                                            ▼             ▼
        │                                    ┌───────────────┐  ┌──────────────────────────────┐
        │                                    │ EffectRunner  │  │           Renderer           │
        │                                    │ spawn/turn/   │  │ tabline · window-tree ·      │
        │                                    │ gitdiff/apply/│  │ transcript‖diff vsplit ·     │
        │                                    │ highlight     │  │ board overlay · statusline · │
        │                                    └──┬────┬────┬──┘  │ cmdrow — cached settled spans│
        │                     ┌─────────────────┘    │    └──────────────┐ └──────────────────┘
        │                     ▼                      ▼                   ▼
        │        ┌────────────────────────┐  ┌────────────────┐  ┌───────────────┐
        │        │  Orchestrator +        │  │  DiffReviewer  │  │  Highlighter  │
        │        │  JobRegistry           │  │  per-turn      │  │  syntect/     │
        │        │  fanout · loop sched ·  │  │  baseline ·    │  │  two-face →   │
        │        │  subagent nesting       │  │  hunk motions ·│  │  tree-sitter  │
        │        └───────────┬────────────┘  │  apply -R      │  │  (cache/hash) │
        │                    │ spawn/attach   └────────────────┘  └───────────────┘
        │                    ▼
        │        ┌────────────────────────────────────────────┐
        │        │        AgentBackend registry (trait)        │
        │        │   ClaudeCodeBackend  [ v1 only impl ]        │
        │        │   builds flags · NDJSON turns · stream-json  │
        │        │   → normalized AgentEvent (Unknown fallback) │
        │        └───────────────────────┬─────────────────────┘
        │                                │ per session → SessionHandle
        │            ┌───────────────────┼───────────────────┐
        │            ▼                   ▼                   ▼
        │   ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
        │   │ SessionHandle A │ │ SessionHandle B │ │ SessionHandle N │
        │   │  reader (parse  │ │   reader        │ │   reader        │
        └───┤  + coalesce +   │ │   writer(stdin) │ │   writer        │
   AppEvent │  fire turn-done)│ │   supervisor    │ │   supervisor    │
            │  writer(stdin)  │ │   (reap group)  │ │   (reap group)  │
            │  supervisor     │ └────────┬────────┘ └────────┬────────┘
            └────────┬────────┘          │                   │
                     ▼                   ▼                   ▼
              claude child A       claude child B       claude child N
              (piped stdio,        own process group    own process group
               own PGID)           killpg on cancel     killpg on cancel
```

The Config loader (TOML → keybindings, theme, defaults, backend registry) feeds `App` at startup and is omitted from the hot-path diagram.

### 3.2 Process model

Each agent session is **one long-lived `claude` child over piped stdio** (not a PTY — stream-json is line-delimited, so a PTY buys nothing and complicates reaping). The child is exec'd by **absolute path**, never through the shell, because the user's shell alias forces `--dangerously-skip-permissions`/`bypassPermissions`, which would silently skip the tool-approval path and defeat diff review.

Verified spawn command (claude 2.1.201):

```
<abs>/claude
  -p
  --input-format  stream-json
  --output-format stream-json
  --verbose                       # REQUIRED: stream-json print mode errors without it
  --include-partial-messages      # token-level content_block_delta lines
  --replay-user-messages          # echoes our sent turns back as acks
  --session-id <uuid avim mints>  # id known up-front → board/resume without scraping init
  --permission-mode acceptEdits   # default; edits apply, reviewed post-hoc via git
  --allowedTools <safe common set># resolves the non-edit-tool auto-deny gap (§6.9)
  --model <alias>
  --add-dir <extra dirs>
```

| Concern | Decision | Ground |
|---|---|---|
| Transport | Piped stdio, `tokio::process::Command` | stream-json is NDJSON; verified live |
| Session id | aeovim self-mints the UUID via `--session-id` | avoids racing `system/init` to learn the id |
| A "turn" | one NDJSON `user` message written to stdin | verified multi-turn on persistent stdin (`num_turns=2`) |
| Skills / slash-commands | a turn whose text begins `/name …` | claude expands it; aeovim does not reimplement |
| Interrupt | prefer control-protocol `interrupt` (keeps child alive, next turn free); `killpg`+`--resume` as hard fallback | see §3.2 interrupt note |
| Idle sessions | resting state is **Detached**; a live child is spawned on first interaction (§8.4) | resolves the idle-demotion question |

Each child is placed in **its own process group**; the supervisor reaps the whole group with `killpg` so a killed agent never orphans MCP servers, hooks, or subagent processes. A `main`-level SIGINT handler and a `Drop` guard on the terminal ensure the alt-screen is restored and groups are reaped even on panic.

**Interrupt is lossy-by-design when done by kill.** Because interrupting a runaway turn is a high-frequency steering action, aeovim prefers the **control-protocol `interrupt`** (`control_request{subtype:"interrupt"}`), which keeps the child alive so the next turn incurs no context-reload cost. This POC is pulled forward from the fast-follow into early implementation precisely because it is the highest-frequency verb (see §5.6). The `killpg`+`--resume` path is the hard fallback: it loses the in-flight assistant turn (claude persists to its jsonl on turn boundaries) and pays a cache-reload cost on resume. When aeovim kills mid-turn it marks the in-flight streaming message `[interrupted]` in its own transcript, and on cold `--resume` it reconciles by trusting the child's next `init`/first turn over aeovim's optimistic partial.

### 3.3 Concurrency model

One tokio multi-thread runtime. Task taxonomy:

| Task | Count | Owns | Blocks on |
|---|---|---|---|
| `UiTask` | 1 | `App`, terminal, `Renderer` | `select!` over 4 sources |
| Reader | 1 / child | that child's `stdout`, its pending-turn queue | `BufReader::lines()` |
| Writer | 1 / child | that child's `stdin` | its own mpsc of turns/controls |
| Supervisor | 1 / child | the child `Child` handle | `child.wait()` |
| Effect | ephemeral | nothing | git/highlight/spawn IO |
| Loop scheduler | 1 / loop job | a `CancellationToken` | `tokio::time::sleep` |

The UI task's core loop — the only place `App` is mutated:

```rust
loop {
    tokio::select! {
        Some(Ok(ev)) = term_events.next() => update(&mut app, AppEvent::Term(ev)),
        Some(msg)     = app_rx.recv()      => update(&mut app, msg),
        _ = tick.tick() => if app.dirty { renderer.draw(&app)?; app.dirty = false; },
        _ = shutdown.cancelled() => break,
    }
    for eff in app.drain_effects() { runner.spawn(eff); }  // runs OFF the UI task
}
```

**Backpressure & coalescing (corrected).** The three requirements — reader always drains the pipe, non-partial events never dropped, memory stays bounded — are not satisfiable with a naive bounded mpsc (a full channel would block `send().await`, stalling the reader, stalling the child). So the reader owns an **unbounded local staging buffer**: it continuously drains stdout, parses each line, and **coalesces consecutive text/thinking deltas for the same content block in place** (bounding memory by *collapse*, not by drop). It forwards to the UI over an unbounded `mpsc` — but coalescing keeps the queue shallow because a burst of deltas becomes one merged `TextDelta`. Non-partial events (init, assistant, tool_use, tool_result, result, rate_limit) are **always forwarded, never dropped**. The authoritative `assistant`/`result` events reconcile the buffer, so if a merged partial is ever superseded before render it is harmless. (Read "bounded" nowhere in this design as "bounded mpsc"; the bound is on retained memory via coalescing.)

**Turn-completion correlation (the load-bearing wire).** The loop scheduler (§4.5) and fan-out permit release (§4.4) both depend on knowing when a specific turn's `result` lands. This is owned by the **reader + a per-session pending-turn FIFO queue** on the `SessionHandle`: `send_turn`/`send_turn_acked` enqueue a completion `oneshot`; the reader, when it emits a normalized `TurnResult`, pops and fires the head oneshot. Turns are strictly sequential per persistent session, so FIFO correlation is valid. On interrupt/kill the supervisor **fires the head oneshot with a `Cancelled` result** so any `select!` waiting on it (loop, fanout) unblocks instead of deadlocking the spawn semaphore.

**Unfocused agents are not special.** A background/loop/fanout agent's reader mutates its buffer and job status exactly like a focused one — it simply doesn't own the viewport, so its updates surface as a tabline spinner and a board row rather than a redraw of the main region.

**Cancellation** is a `tokio_util::CancellationToken` tree: cancelling a job cancels its children and the writer sends interrupt / the supervisor `killpg`s. A **spawn semaphore** caps concurrent children (default in config), and a **watchdog** flags a child whose reader has been silent past a threshold.

### 3.4 State model

Single-owner Elm state. `update` does no IO; it mutates `App` and returns `Effect`s.

```rust
new_key_type! { pub struct AgentId; pub struct JobId; pub struct WinId; }

pub struct App {
    agents:  SlotMap<AgentId, Agent>,
    jobs:    SlotMap<JobId, Job>,
    tabs:    Vec<Tab>,
    focus:   Focus,               // (tab index, WinId)
    mode:    Mode,
    pending: PendingInput,
    cmdline: String,
    board_open: bool,
    config:  Config,
    dirty:   bool,
    effects: Vec<Effect>,
}

pub enum Mode { Normal, Insert, Visual(VisualKind), OperatorPending, Command }

pub struct Tab { name: String, root: WinNode }              // one agent's home layout
pub enum WinNode {
    Leaf(Window),
    Split { dir: SplitDir, children: Vec<WinNode> },        // v1: one vsplit (2 leaves)
}
pub struct Window { id: WinId, view: View, scroll: usize, cursor: usize }
pub enum View { Transcript(AgentId), Diff(AgentId), Board } // a window can view ANY agent

pub struct Agent {                                          // buffer ≈ one session
    session_id: Uuid,
    backend:    BackendId,
    model:      String,
    perm_mode:  PermMode,
    cwd:        PathBuf,
    worktree:   Option<PathBuf>,   // fanout/parallel agents get their own
    baseline:   BaselineRef,       // per-turn diff baseline (git snapshot; §6.3)
    transcript: Vec<Message>,
    streaming:  Option<Message>,   // in-flight assistant msg; replaced on `assistant`
    palette:    Capabilities,
    status:     AgentStatus,
    cost_usd:   f64,
}

pub struct Message {
    role:  Role,
    blocks: Vec<Block>,            // Text | Thinking | ToolUse | ToolResult
    hash:  u64,                    // content hash → settled-span cache key
    spans: Option<Arc<Vec<Line<'static>>>>, // memoized ONLY once settled (§3.5/§7.5)
}
```

**v1 window scope:** each tab is one agent's home; a tab may hold one vsplit (transcript‖diff of the same agent, or transcript‖another-agent). Because a `Window` views any `AgentId`, side-by-side conversations need only pick which agent each leaf shows. Arbitrary N-way trees are a fast-follow, but `WinNode` is already a tree so the layout code doesn't change.

**Close vs kill.** `View` closing (`:q`, `<C-w>c`) only removes a viewport / detaches the agent — the child keeps running and stays on the board. Reaping the child is the explicit `:bd!`/`<leader>K`, mirroring vim's hide-vs-`:bd` distinction so you never accidentally kill in-flight work to clear screen clutter.

### 3.5 Rendering loop

`Renderer` reads `&App` and paints with ratatui over crossterm in alt-screen + raw mode. Redraw discipline (enforced from day one):

- **Draw only on the tick, only when `dirty`.** Streaming sets `dirty`; the 40fps tick coalesces a burst of deltas into one repaint.
- **Settled-message span cache (single mechanism).** A `Message`'s highlighted `Vec<Line>` is built once and memoized *only after the message settles* (its final content hash is stable). The **in-flight streaming message renders on a separate non-cached fast path** — rebuilt each dirty tick, never inserted into the cache — and is committed to the cache exactly once when the authoritative `assistant`/`result` event finalizes it. The settled cache is an **LRU** (bounded) keyed by final content hash. This resolves the earlier contradiction between the hash-map cache and `OnceCell`: there is one cache, it never gains an entry per token, and the streaming message is never cached mid-growth.
- **Viewport virtualization.** Only lines intersecting the visible window are laid out; ratatui then diffs its back buffer against the terminal, so unchanged rows cost nothing.
- **Board is an overlay**, not a separate render pass — a `Table` widget drawn over the main region when `board_open`.

Highlighting is staged: **syntect + two-face first** (no C toolchain), then **tree-sitter for pinned core languages** (§7). tree-sitter is for code inside conversations and diffs — *not* for tab management.

### 3.6 End-to-end data-flow walkthroughs

**A. Keystroke → action → effect**

```
crossterm EventStream yields KeyEvent
  → KeymapEngine walks the (mode, PendingInput) trie
      ↳ Pending  → arm timeoutlen, wait for next key
      ↳ Action(a)→ update(App, Action) mutates App, pushes Effect(s)
  → UiTask drains effects → EffectRunner spawns them off-thread
      e.g. SendTurn → SessionHandle.writer writes one NDJSON user msg + enqueue turn-done oneshot
```

**B. Stream line → buffer → pixels** (the hot path)

```
claude child emits {"type":"stream_event", event:{content_block_delta, text_delta}}
  → reader parses line, coalesces with prior deltas for this (message-id, block-index),
    ALWAYS drains the pipe, forwards AppEvent::Agent{id, TextDelta{...}} over unbounded mpsc
  → UiTask update(): append chunk to agents[id].streaming, set dirty=true
  → next 40fps tick: Renderer rebuilds spans for ONLY the streaming message
    (settled messages served from LRU cache), ratatui diffs back buffer
  → on {"type":"assistant"} update() replaces streaming with authoritative msg → cache insert
  → on {"type":"result"} reader fires head turn-done oneshot; update() closes the Job,
    adds total_cost_usd, board→green
```

**C. Turn result → diff review** (per-turn git baseline)

```
result event for a turn
  → update() pushes Effect::GitDiff{ agent, baseline }
  → EffectRunner shells `git diff <baseline>` (per-turn snapshot, not raw HEAD; §6.3)
  → AppEvent::DiffReady{ agent, hunks } → DiffReviewer stores hunks; re-anchors cursor by HunkId
  → user navigates ]c/[c, visual-selects hunks; `s` approve (avim-side mark), `x` reject
      reject → Effect::ApplyReverse (git apply -R the one-hunk patch)
  → gitignore-aware, debounced notify watcher fires on real file change → Effect::GitDiff refresh
```

**D. Loop scheduling** (aeovim-owned, not claude's `/loop`)

```
loop scheduler task:
  sleep until next_time (interval, or self-paced FLOOR delay)
  → acquire spawn-semaphore permit
  → SessionHandle.send_turn_acked(prompt, done_tx)   (claude --resume state on disk)
  → await done_tx (fired by reader on this turn's `result`, or Cancelled on kill)
  → spent += result.cost_usd; if spent >= cost_cap (mandatory for self-paced): break
  → compute next_time; check completion sentinel; repeat
cancel = token.cancel() + killpg; pause = hold before acquiring the permit
```

### 3.7 The adapter seam (backend trait)

Everything above the seam speaks normalized events; only `ClaudeCodeBackend` knows stream-json exists.

```rust
pub trait AgentBackend: Send + Sync {
    fn spawn(&self, cfg: SessionCfg) -> anyhow::Result<SessionHandle>;
    fn attach(&self, session_id: Uuid) -> anyhow::Result<SessionHandle>; // cold resume
}

pub struct SessionHandle {
    pub events: mpsc::Receiver<AgentEvent>,   // normalized, drained by the reader task
    turn_tx:    mpsc::Sender<Turn>,           // → writer owns child stdin
    ctrl_tx:    mpsc::Sender<Control>,        // interrupt live in early impl; canUseTool fast-follow
    pending:    PendingTurns,                 // FIFO of completion oneshots (§3.3)
    cancel:     CancellationToken,
}

pub enum AgentEvent {
    Init { model: String, perm_mode: String, tools: Vec<String>, skills: Vec<String>,
           slash_commands: Vec<String>, subagents: Vec<String>, mcp: Vec<McpServer>,
           version: String },
    TextDelta   { msg_id: String, block_idx: usize, chunk: String }, // correlate by (id, idx)
    Assistant   { msg: Message },
    ToolUse     { id: String, name: String, input: serde_json::Value,
                  parent_tool_use_id: Option<String> },
    ToolResult  { tool_use_id: String, content: serde_json::Value },
    PermissionRequest { tool: String, input: serde_json::Value }, // control-protocol, fast-follow
    TurnResult  { cost_usd: f64, denials: Vec<Denial>, stop: StopReason },
    RateLimit   { resets_at: Option<String> },
    Raw(serde_json::Value),                   // unknown `type` — never fatal
}
```

`ClaudeCodeBackend` deserializes stream-json with a **tag-based serde enum (`tag = "type"`) plus a `#[serde(other)] Unknown` variant**, keeps the raw line on parse error, and version-gates on `Init.version`. A future `CodexCliBackend` or direct-API backend is a new `impl AgentBackend` — `:model` stays within a backend; switching agent tooling swaps the backend impl.

---

## 4. Orchestration: Parallel Tasks, Fan-out, Skills & Loops

aeovim is an orchestrator because every unit of agent work is reified as one **Job** with state, cost, and a cancel handle, and every Job is visible and drivable from a quickfix-style **Task Board**. The guiding rule: *wrap, don't reimplement.* Claude Code already runs skills, subagents, MCP tools, hooks, and context management inside each child; aeovim inherits those by parsing the stream-json feed. aeovim implements only the *multiplex* layer the CLI has no notion of: a fan-out spawner (with harvest), an aeovim-owned loop scheduler, and the Job registry/board.

### 4.1 The Job — aeovim's backend-agnostic unit of work

```rust
slotmap::new_key_type! { pub struct JobId; pub struct AgentId; pub struct GroupId; }

pub struct Job {
    pub id:        JobId,
    pub kind:      JobKind,
    pub agent:     AgentId,
    pub parent:    Option<JobId>,    // subagents / fan-out members nest here
    pub label:     String,
    pub state:     JobState,
    pub started:   Instant,
    pub last_line: String,
    pub cost_usd:  f64,              // summed from result events (authority for budget)
    pub budget:    Option<f64>,      // avim soft cap; job cancels itself on breach
    pub cancel:    CancellationToken,
}

pub enum JobKind {
    Turn,
    Loop     { schedule: Schedule, runs: u32 },
    Fanout   { group: GroupId, idx: usize },
    Subagent { tool_use_id: String },
    Background,
}

pub enum JobState {
    Queued, Running, Blocked(Blocked), Done, Failed(String), Cancelled,
}
pub enum Blocked { Permission, NeedsInput, RateLimited }
```

**Lifecycle.** State is *derived from the event stream*, never polled (the one exception, background `--bg` agents, is §4.6).

```
                 spawn permit
   [Queued] ─────────────────▶ [Running] ───────result:success───▶ [Done]   (green)
      │                          │  ▲                                  │
      │                          │  │ resume / next tick               │ Loop: re-arm
      │                    ┌─────┘  └──────────┐                       ▼
      │              rate_limit /         permission /            [Queued]───┐
      │              perm_denial          set_permission_mode        ▲       │
      │                    ▼                                          └───────┘
      │                [Blocked] ──approve+retry / rule──▶ [Running]
      │                    │
      └────token.cancel()──┴──result:error──▶ [Failed] (red) / [Cancelled] (dim)
```

| Normalized event | Transition | Confidence |
|---|---|---|
| `send_turn` issued | `Queued`/`Running` | verified |
| `system{subtype:status,"requesting"}` | → `Running` | verified |
| `stream_event` (content_block_delta) | stay `Running`, append | verified |
| `assistant{message}` | commit message, stay `Running` | verified |
| `rate_limit_event` | → `Blocked(RateLimited)` | verified |
| `result{success}` + `permission_denials[]` non-empty | → `Blocked(Permission)` (surfaces would-be edit + denied tool) | verified |
| `control_request{can_use_tool}` (manual gate) | → `Blocked(Permission)` | **verify** — control handshake unverified; fast-follow |
| `result{success}`, no denials | → `Done` (Loop → `Queued`) | verified |
| `result{is_error}` / `subtype:error_*` | → `Failed(reason)` | verified |
| `token.cancel()` + `killpg` | → `Cancelled` | verified |

`cost_usd` comes off `result.total_cost_usd`; **aeovim's summation is the budget authority** for loops/fan-out (cancel via token). `--max-budget-usd` is set on the child only as a *coarse hard backstop, well above* aeovim's soft cap — a CLI budget abort surfaces as `result:error` → red job, and aeovim's soft cap should trip first (see §8.5).

### 4.2 Inherited vs. implemented

| Capability | Owner | How aeovim gets/surfaces it | Confidence |
|---|---|---|---|
| Skills / slash-commands | **claude** | enumerated in `system/init`; invoked by a slash-prefixed user turn | verified |
| Subagents (Task tool) | **claude** | arrive with `parent_tool_use_id`; nested as `JobKind::Subagent` | verified |
| MCP tools | **claude** | auto-discovered at startup; appear as `tool_use`/`tool_result` | verified |
| Hooks, context mgmt | **claude** | fire inside the child; reflected in the stream | verified |
| Multi-turn / resume | **claude** | one long-lived child; `--resume` as cold-attach | verified |
| **Fan-out** (1→N) | **aeovim** | N spawns, each in its own worktree, one Job per member, harvestable | verified (CLI has no fan-out) |
| **Loops** | **aeovim** | aeovim-owned tokio scheduler — **not** claude's `/loop` | verified (§4.5) |
| **Task board** | **aeovim** | `SlotMap<JobId, Job>` + overlay UI | n/a |
| Background detached | **aeovim wraps claude** | `claude --bg` + `claude agents --json` polling | **verify** — flags/JSON version-sensitive |

The load-bearing verified fact: **each `claude -p` is an independent process with no multiplex or scheduling primitive**, and **`/loop`'s recurring behavior is TUI-only** — a `-p` child answers once and exits, so the interval never fires. Those two gaps are what aeovim fills.

### 4.3 Skills & slash-commands (inherited)

At `system/init` aeovim parses `slash_commands`, `skills`, `agents`, `mcp_servers` into a **per-agent capability palette**. Invocation needs no special flag: a skill is a user turn beginning with `/`.

```rust
fn invoke_skill(h: &SessionHandle, name: &str, args: &str) {
    h.send_turn(format!("/{name} {args}").trim_end().to_string());
}
```

`<leader>s` opens the fuzzy palette; `:skill`/`:cmd` dispatch directly. A skill invocation is itself a `JobKind::Turn` on the board (labelled with the skill name); any subagents it spawns nest beneath it via `parent_tool_use_id`.

### 4.4 Parallel tasks & fan-out (implemented)

Fan-out sends **one prompt to N sessions**, each editing member in **its own git worktree** so concurrent `acceptEdits` agents cannot clobber a shared tree (via `git worktree add` + child cwd; a `claude -w/--worktree` flag is *verify:* per `--help` — aeovim falls back to managing worktrees itself if unreliable). Concurrency is bounded by a spawn semaphore; the permit is released when the member's turn-done oneshot fires (§3.3).

```rust
async fn fanout(orc: &Orchestrator, prompt: String, spawns: Vec<SessionCfg>) -> GroupId {
    let group = orc.new_group(prompt.clone(), spawns.len());
    for (idx, cfg) in spawns.into_iter().enumerate() {
        let permit = orc.sem.clone().acquire_owned().await;
        let agent  = orc.backend.spawn(cfg.in_own_worktree()).await?;
        let job    = orc.register(JobKind::Fanout { group, idx }, agent, prompt.clone());
        let done   = orc.send_turn_acked(agent, prompt.clone(), job.cancel.child_token());
        orc.hold_permit_until(done, permit); // permit freed when reader fires the oneshot
    }
    group
}
```

**Harvest ships in v1, not deferred.** Fan-out's whole point is "try N approaches, pick the winner," so the board offers minimal harvest alongside spawn:

- `gm` — **adopt this member**: `git merge`/cherry-pick its worktree branch into the main tree.
- `gd` — **discard**: `git worktree remove --force`.

Conflict resolution stays crude initially (on a merge conflict, bail to a message and leave the worktree for manual resolution), but the pick-a-winner action exists in v1. The board shows a collapsible group header (`fanout/N`) with `done/total` and aggregate cost; each member is a jumpable child row.

```
:fanout 3 "add rate-limit middleware, three approaches"
:fanout auth-a,auth-b "port this test to your style"   # explicit target sessions
```

> **Cost warning surfaced in the UI:** N members = ~N× token spend on one subscription (verified: quota is per-session; a trivial reply already cost ~$0.079 due to context reload). The board's aggregate cost line plus the spawn-semaphore cap are the guardrails.

### 4.5 Loops / recurring prompts (implemented)

aeovim **does not** delegate recurrence to claude's `/loop` (TUI-only; a `-p` child exits before the interval fires — verified). A loop is an aeovim-owned tokio task that re-issues turns against a persistent session and reports each iteration as a Job tick — pausable, cost-capped, cancellable from the board.

```rust
pub enum Schedule {
    Every(Duration),
    SelfPaced { floor: Duration },   // minimum inter-iteration delay (config default 30s)
}

async fn run_loop(h: SessionHandle, sched: Schedule, prompt: String,
                  cancel: CancellationToken, cost_cap: f64,  // MANDATORY for SelfPaced
                  tx: mpsc::Sender<AppEvent>) {
    let mut spent = 0.0;
    loop {
        let wait = match sched {
            Schedule::Every(d)            => d,
            Schedule::SelfPaced { floor } => floor,   // never 0 — no busy loop
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
        let (done_tx, done_rx) = oneshot::channel();     // fired by reader on this turn's result
        h.send_turn_acked(prompt.clone(), done_tx);
        let res = tokio::select! {
            _ = cancel.cancelled() => break,
            r = done_rx => r.ok(),
        };
        if let Some(r) = res {
            spent += r.cost_usd;
            tx.send(AppEvent::LoopTick { cost: r.cost_usd, spent }).await.ok();
            if spent >= cost_cap { break; }                     // hard cost cap
            if r.completion_sentinel_seen { break; }            // model signalled done
        }
    }
}
```

- `:loop 5m <prompt|/cmd>` start · `:loop stop` · `<leader>l`.
- Interval loops use a tokio timer; **self-paced** loops re-fire after a **configurable floor delay** (default 30s) so they can't busy-loop the API. The cost cap is **mandatory** for self-paced.
- **Completion protocol:** the self-paced loop prompt instructs the agent to emit a specific sentinel line (or a defined `stop_reason`); the loop stops when the sentinel is seen or the cap trips. The board shows next-fire time and accumulated cost.
- Cancellation is `token.cancel()`; the next `select!` arm wins immediately.
- *Escape hatch:* the user can send `"/loop 5m /foo"` as a plain turn to get claude's native loop — but that is fire-and-forget, **not** a first-class aeovim board Job. aeovim's scheduler is the recommended path.

> Under `acceptEdits`, non-edit tools (e.g. `Bash`) still gate and auto-deny headless (§6.9), which can stall a loop. aeovim's default `--allowedTools` set covers the common safe commands; anything else surfaces as a **yellow "approve+retry" board row** rather than hanging silently.

### 4.6 Subagents & background (inherited / wrapped)

**Subagents** need no aeovim work: Task-tool output arrives tagged with `parent_tool_use_id`; the reducer attaches a `JobKind::Subagent` child under its parent Job and renders it indented (verified).

**Background agents** are a *fast-follow*, not v1-core. `claude --bg` detaches a session and `claude agents --json` lists live sessions. aeovim can seed the board from these — including sessions it did not spawn — but they **do not stream**, so they require **polling** on an interval (the only place aeovim polls). Flags and JSON shape here are *verify: likely, version-sensitive* — gate on `claude_code_version` and degrade gracefully. (`:ls` in v1 lists only aeovim-owned sessions; the claude-agents merge is gated behind this same version check.)

### 4.7 The Task Board — watch, cancel, jump

A dedicated overlay buffer (`:copen`-style), a `ratatui` `Table`. Status uses the attention model — **green = done, yellow = needs-input/blocked, red = error** — plus a spinner for `Running`, dim for `Cancelled`.

```
┌ Task Board ─────────────────────────────────────────── 7 jobs · $1.94 ┐
│  ● id     kind       agent          state        elapsed  cost  last   │
│  ● j3     turn       api-refactor   running      0:12    $0.08  Edit…  │
│  ● j4     fanout/3   auth-a         done         1:40    $0.31  3 files│
│  ● j5     fanout/3   auth-b         blocked:perm 1:38    $0.29  Bash?  │
│  ○ j6     fanout/3   auth-c         queued       —       —      —      │
│  ● j7     loop       deploy-watch   queued 4m12s  run#7  $0.44  ok     │
│    └ j9   subagent   (Explore)      running      0:03    —      Grep…  │
│  ● j8     turn       scratch        error        0:05    $0.01  429    │
└ ]j/[j move · <CR> jump · a approve+retry · dd cancel · r restart · gm/gd ┘
```

| Key / command | Action |
|---|---|
| `<leader>t` / `:tasks` / `:board` | toggle the board overlay |
| `]j` / `[j` | next / prev Job row |
| `<CR>` | **jump**: focus that Job's agent buffer (and its diff) |
| `a` | **approve+retry** a `blocked:perm` job — re-issue the denied tool with it allowed (or send the pre-apply allow when the control gate is live) |
| `dd` | cancel Job (`token.cancel()` + `killpg`) |
| `r` | restart / re-issue the Job's turn |
| `<leader>c` / `Ctrl-c` | interrupt the *focused* agent's in-flight turn |
| `gm` / `gd` | fan-out: adopt member (merge) / discard member worktree |

Jumping is cheap — a Job knows its `agent`; focusing swaps the viewport. Watching needs no polling for streaming Jobs — the board redraws on the same dirty-flag/frame-tick as everything else.

### 4.8 Concurrency & cancellation plumbing

All of the above rides the single-writer IO spine (§3). Orchestration adds three primitives:

- **Cancellation-token tree.** Root → per-agent → per-Job → per-turn. Cancelling a fan-out group cancels every member; cancelling aeovim cancels everything. Cancel = cooperative `token.cancel()` **plus `killpg`** of the child's group (avoids orphaned Node + MCP subprocesses). On cancel, the pending-turn oneshot is fired `Cancelled` so waiters unblock.
- **Spawn semaphore.** Bounds simultaneous heavyweight children; permits held from spawn until the member's `result`.
- **Watchdog.** Flags Jobs stuck in `Running`/`Blocked` past a threshold so a silently-dead loop or stalled MCP startup turns yellow rather than lying green.

---

## 5. Agent Abstraction & the Claude CLI Adapter

aeovim never lets stream-json leak past a single module. The UI, input engine, and orchestrator only ever see a *normalized* event/command vocabulary; everything Claude-specific lives behind one trait.

```
 ┌──────────── UI task / update() / Orchestrator ────────────┐
 │  sees only: AgentEvent (normalized) + Control (normalized)  │
 └───────────────────────────▲───────────────┬───────────────┘
                             │ mpsc          │ send_turn / send_control
 ┌───────────────────────────┴───────────────▼───────────────┐
 │  dyn AgentBackend  →  SessionHandle   (the ADAPTER SEAM)     │
 ├─────────────────────────────────────────────────────────────┤
 │  ClaudeCodeBackend (v1)  │  CodexCliBackend*  │  DirectApi*   │
 │  stream-json <-> AgentEvent   *future, unimplemented          │
 └─────────────────────────────────────────────────────────────┘
                 child `claude -p` over piped stdio
```

### 5.1 The trait seam

```rust
#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn id(&self) -> &'static str;                 // "claude-code"
    async fn spawn(&self, cfg: SessionCfg) -> Result<SessionHandle>;
    async fn attach(&self, id: SessionId, cfg: SessionCfg) -> Result<SessionHandle>;
}

pub struct SessionCfg {
    pub session_id: SessionId,      // avim-minted UUID (§5.2)
    pub model: Option<String>,
    pub permission_mode: PermMode,
    pub cwd: PathBuf,
    pub add_dirs: Vec<PathBuf>,
    pub allowed_tools: Vec<String>, // default safe set for non-edit tools (§6.9)
    pub worktree: Option<PathBuf>,
}

pub struct SessionHandle {
    pub id: SessionId,
    pub events: mpsc::Receiver<AgentEvent>,
    tx_turn: mpsc::Sender<TurnInput>,
    tx_ctrl: mpsc::Sender<Control>,           // interrupt wired early; canUseTool fast-follow
    pending: PendingTurns,                    // FIFO completion oneshots (§3.3)
    cancel: CancellationToken,
}

impl SessionHandle {
    /// Enqueue a turn AND a completion oneshot popped+fired by the reader on TurnResult.
    pub fn send_turn(&self, t: TurnInput);
    pub fn send_turn_acked(&self, t: TurnInput, done: oneshot::Sender<TurnDone>);
    pub fn send_control(&self, c: Control);
    pub async fn shutdown(self);              // token.cancel() + killpg; fires pending Cancelled
}
```

`send_turn_acked` + the reader's pending-turn FIFO are what let the loop scheduler and fan-out know when a specific turn finished (§3.3) — the single most important cross-boundary wire in the design.

### 5.2 ClaudeCodeBackend — exact flags

Verified against `claude --help` (v2.1.201) and a live headless run. aeovim execs the **binary by absolute path**, never the shell alias (the alias forces `bypassPermissions`, defeating diff review). Resolve the path once at startup (`which claude` / known versions dir).

```
<abs>/claude -p \
  --input-format  stream-json \
  --output-format stream-json \
  --verbose \                     # REQUIRED with -p + stream-json (verified error otherwise)
  --include-partial-messages \    # token-level content_block_delta events
  --replay-user-messages \        # echoes our user msgs back as ack
  --session-id <avim-minted-uuid> \
  --permission-mode acceptEdits \ # per SessionCfg; fanout agents get a worktree too
  --allowedTools "Bash(cargo *)" "Bash(git status:*)" Read Grep ... \  # §6.9
  --model sonnet \
  --add-dir <extra dirs...>
```

| Flag | Purpose | Confidence |
|------|---------|-----------|
| `-p/--print` | Non-interactive headless | verified |
| `--output-format stream-json` | NDJSON event stream | verified |
| `--verbose` | **Mandatory** with `-p`+stream-json | verified |
| `--input-format stream-json` | Keep stdin open; one NDJSON `user` msg per turn | verified (multi-turn, `num_turns=2`) |
| `--include-partial-messages` | Adds `stream_event` lines wrapping Anthropic SSE deltas | verified |
| `--replay-user-messages` | Echoes user turns for send-ack/ordering | verified |
| `--session-id <uuid>` | aeovim mints the id up front | verified |
| `--permission-mode <m>` | `acceptEdits\|plan\|default\|bypassPermissions` | verified (name set; `manual`/`auto` — **verify** against pinned `--help`) |
| `--allowedTools <patterns>` | Pre-allow safe non-edit tools so headless turns don't auto-deny | verified flag; the *default set* is config (§6.9) |
| `--model <alias\|id>` | `fable\|opus\|sonnet` or full id | verified flag; alias set **verify** |
| `--add-dir <path>` | Extra readable/editable dirs | verified |
| `--effort <low..max>` | Optional reasoning effort | verified (present) |
| `--max-budget-usd <n>` | Coarse per-child hard backstop (set above aeovim's soft cap) | verified (present) |

Transport is **plain piped stdio**, not a PTY. Invoking a skill/slash-command needs no flag: it's a user turn starting with `/` (§5.5).

### 5.3 The stream-json event enum aeovim parses

Output is JSONL keyed on top-level `type` (then `subtype`). Every line carries `session_id` and a `uuid`; `parent_tool_use_id != null` marks subagent output. Parse with an **internally-tagged serde enum plus an `Unknown` catch-all**, always retaining the raw line so an unrecognized `type` is non-fatal.

**Field-casing correctness (must-fix in the skeleton spike).** `#[serde(rename_all = "snake_case")]` on an internally-tagged enum renames only *variant tags*, **not struct fields** — and the live init line **mixes casing** (`permissionMode` is camelCase while `mcp_servers`/`slash_commands` are snake_case). So every field must carry its **exact** JSON key via explicit `#[serde(rename = "...")]`, verified against a captured fixture with a round-trip unit test. Do not rely on a blanket `rename_all` for fields.

```rust
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeEvent {
    System(SystemEvent),
    StreamEvent {                 // present only with --include-partial-messages
        event: SseDelta,          // message_start | content_block_delta | ...
        #[serde(default)] parent_tool_use_id: Option<String>,
        #[serde(default)] ttft_ms: Option<u64>,
    },
    Assistant {
        message: AnthropicMessage,
        #[serde(default)] parent_tool_use_id: Option<String>,
    },
    User { message: UserMessage },
    Result(ResultEvent),
    RateLimitEvent { rate_limit_info: RateLimitInfo },
    #[serde(other)]
    Unknown,                      // forward-compat: keep raw line, log, don't crash
}

#[derive(Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum SystemEvent {
    Init {                        // EXACT keys — capture a fixture and annotate each
        session_id: String,
        cwd: String,
        model: String,
        #[serde(rename = "permissionMode")] permission_mode: String, // camelCase in the wild
        tools: Vec<String>,
        slash_commands: Vec<String>,
        skills: Vec<String>,
        agents: Vec<String>,
        mcp_servers: Vec<McpServer>,
        #[serde(rename = "claude_code_version")] version: String,    // confirm exact key
    },
    Status { status: String },
    HookStarted { /* ... */ },
    HookResponse { /* ... */ },
}

#[derive(Deserialize)]
pub struct ResultEvent {
    pub subtype: String, pub is_error: bool, pub num_turns: u32,
    pub total_cost_usd: f64, pub usage: Usage,
    #[serde(default)] pub permission_denials: Vec<PermissionDenial>, // tool_name + full tool_input
    #[serde(default)] pub stop_reason: Option<String>,
}
```

The reader maps `ClaudeEvent → AgentEvent` (§3.7). **Delta correlation (must-verify):** `stream_event` wraps raw Anthropic SSE, where deltas correlate by content-block `index` under a preceding `message_start` id — the outer envelope's per-line `uuid` is likely *not* per-message. Coalescing therefore groups by **(message id from `message_start`, content_block `index`)**, not the envelope uuid. Capture a multi-block streaming turn in the spike and confirm the key before wiring §3.3 coalescing.

Reconciliation: live `TextDelta`s update the streaming message optimistically; the authoritative `AssistantMsg`/`TurnResult` *replace* it. `Init.claude_code_version` is the **version gate** — on drift, warn and degrade rather than trust field shapes.

### 5.4 Multi-turn follow-ups

Because stdin stays open, a follow-up turn is one more NDJSON `user` line (verified: two turns kept context, `num_turns=2`, one stable `session_id`). Both content shapes are accepted:

```jsonc
{"type":"user","message":{"role":"user","content":"refactor the parser"}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"refactor the parser"}]}}
```

The per-child **writer task** owns `ChildStdin`; `send_turn` pushes onto its channel and enqueues the completion oneshot (§5.1). **Cold re-attach** uses `claude -p --resume <session-id> …`; `--fork-session` branches for fanout. Persistent-child is preferred for streaming + interrupt; `--resume`-per-turn is the documented fallback.

### 5.5 Skills & slash-commands

Inherited, not reimplemented. The `system/init` line enumerates `skills`, `slash_commands`, `agents`, `mcp_servers`; aeovim populates a per-agent palette. Invocation is a user turn starting with `/`:

```jsonc
{"type":"user","message":{"role":"user","content":"/deep-research treesitter ABI skew"}}
```

Claude expands the skill and executes it; work streams back as ordinary events. aeovim's loop scheduler is deliberately aeovim-owned rather than delegating to `/loop` (which needs an idle interactive session and dies under `-p`).

### 5.6 Model switching (`:model`), interrupt, and the control protocol

`:model opus` / `:effort high` change `SessionCfg` for the focused agent. Two paths:

| Path | Mechanism | Status |
|------|-----------|--------|
| **Restart + resume** | `shutdown()` the child, respawn with new `--model` and `--resume <session-id>` | **v1 fallback — fully verified** |
| **Live control** | `control_request{subtype:"set_permission_mode"/model}` on stdin | **verify — prototype early** |

**Both `:model` and `:perm` are frequent actions, so restart+resume is *not* the silent default.** Restart pays respawn latency AND a full-context cache reload. Mitigations shipped in v1: **(a)** the UI warns when a switch will restart+reload (cost + latency); **(b)** if no turn is in flight, aeovim applies the new model/mode to the **next turn's flags** without an immediate respawn where the flag is per-turn expressible; **(c)** the live-control POC (`set_permission_mode`, and especially `interrupt`) is prototyped **early**, not deferred — the binary contains the machinery (and a `"set_permission_mode is not supported in this context"` string means *verify*, not *assume-impossible*).

**Interrupt** is the highest-frequency steering verb, so the control-protocol `interrupt` (keeps the child alive, next turn free) is pulled forward from the fast-follow. `start_kill`+`killpg`+resume remains the hard fallback and is lossy-by-design (§3.2).

The binary ships the Agent-SDK **control protocol** over the same channel — `control_request`/`control_response` with subtypes `initialize`, `can_use_tool`, `set_permission_mode`, `interrupt`, `hook_callback`; responses carry `behavior: allow|deny`, `updatedInput`, `request_id`. This is the seam for the **manual diff gate** (§6.2). In v1 the `Control` enum exists; `interrupt` is wired early, `can_use_tool` routing is the fast-follow:

```rust
pub enum Control {
    Initialize { can_use_tool: bool },   // MUST advertise canUseTool to route it (see below)
    CanUseToolResponse { request_id: String, behavior: Behavior, updated_input: Option<Value> },
    SetPermissionMode(PermMode),
    Interrupt,                            // wired early
}
```

**Uncertain claims, flagged honestly:**

| Uncertain claim | Why flagged | Handling |
|---|---|---|
| Exact `control_request`/`control_response` wire envelope | Inferred from binary strings + public SDK | POC before committing the manual gate |
| **`initialize` handshake is what *routes* `can_use_tool` to aeovim** | A probe auto-denied because it skipped `initialize` — the mode alone did not route the callback | Prototype the handshake first; per-call gating depends on it, **not** on a permission-mode name |
| Which permission mode leaves edits un-pre-approved | *verify:* prior art uses `default`; `manual` may be an alias of `default`; `manual` reportedly only prompts *first use* of each tool | Confirm the canonical mode name live before relying on it |
| Live `set_permission_mode`/`model` on a running child | Binary contains a "not supported in this context" string | Assume restart+resume until proven |
| `--input-format stream-json` + control protocol stability | Undocumented, version-sensitive | Version-gate on `init`; keep post-apply git-diff fallback |

Until the handshake is proven, the diff-review story stands on the **verified** fallback: `acceptEdits` + post-apply git diff (§6), plus the verified fact that even in `default` mode denied edits return in `result.permission_denials[]` *with full `tool_input`* — a cheap preview channel needing no control protocol.

### 5.7 Future adapters plug in here

```rust
struct CodexCliBackend { bin: PathBuf }
#[async_trait]
impl AgentBackend for CodexCliBackend {
    fn id(&self) -> &'static str { "codex-cli" }
    async fn spawn(&self, cfg: SessionCfg) -> Result<SessionHandle> { /* codex flags; map stream */ }
    async fn attach(&self, id: SessionId, cfg: SessionCfg) -> Result<SessionHandle> { /* ... */ }
}
```

Claude-only richness (`parent_tool_use_id`, `permission_suggestions`, cache-token usage) rides as **optional** fields on normalized events, so a leaner backend leaves them `None`. The UI, keymap engine, and orchestrator compile against `dyn AgentBackend` / `AgentEvent` and never learn which agent is on the other end.

---

## 6. Diff Review & Edit-Approval Model

### 6.1 The core constraint

Claude Code applies file edits *itself*. When an agent runs `Edit`/`Write`/`MultiEdit`, the change lands the moment the permission check passes — there is no native "staging" layer aeovim can slot into. Critically, **Claude gates permission per whole tool call, not per hunk**: one `Edit` can rewrite a 200-line region as a single approve/deny unit, `MultiEdit` bundles several edits under one decision, `Write` is a whole-file decision. There is no wire-level "approve hunk 2, reject hunk 3."

The only place true per-hunk granularity exists is **git**. So aeovim inverts the naive dream: let edits land, then treat git as the review surface.

### 6.2 Chosen approach

**Primary (v1): Post-apply git review against a per-turn baseline.** Editing agents run under `--permission-mode acceptEdits`. Edits apply immediately; on each turn `result` aeovim diffs the working tree against a **per-turn baseline** (not raw HEAD — §6.3), parses hunks with `similar`, and gives vim-native hunk motions plus git-backed approve/reject.

**Secondary (fast-follow): Pre-apply manual gate** via the control protocol (`can_use_tool`). Intercepts a *whole* edit before it applies (coarse, per-call, not per-hunk) and depends on an unverified handshake — deferred.

| | Post-apply git review (v1 primary) | Pre-apply manual gate (fast-follow) |
|---|---|---|
| Granularity | **Per-hunk** (git) | Per whole tool call |
| When | After edit lands | Before edit applies |
| Mechanism | per-turn baseline + `similar` + `git apply -R` | control protocol `can_use_tool` → `behavior: allow\|deny` |
| Permission mode | `acceptEdits` | a mode that doesn't pre-approve edits **+ `initialize` advertising canUseTool** (§5.6) |
| Protocol risk | None — plain git + read stdout | High — control envelope + handshake unverified |
| Reject fidelity | Reverse-patch selected hunks | Deny entire call, optional re-prompt |
| Ships in | v1 | after the control round-trip is proven |

Why lead with post-apply: **(a)** `can_use_tool` routing requires an `initialize` handshake whose wire shape is inferred, not exercised — betting v1's headline feature on it is reckless; **(b)** even working, its granularity is per-call, so "reject one hunk" degrades to deny-and-re-prompt; **(c)** `acceptEdits`/allow-rules skip `can_use_tool` for edits entirely. Git already models hunks natively and can't drift.

> Even in plain `default` mode, denied edits surface in `result.permission_denials[]` **with the full `tool_input`** — a free preview payload that makes the manual gate cheap to graft later.

### 6.3 Baselines and worktree isolation

Because edits apply immediately, two hazards must be handled:

**Solo-agent WIP pollution (must-fix).** The common case is one agent in your repo's main tree, which is usually *already dirty* with your own WIP. Diffing raw `HEAD`-vs-worktree would mix your uncommitted changes with the agent's edits, and `]c`/`s`/`x` would operate on that polluted set. So the solo agent records a **per-turn baseline snapshot** at turn *start* — a `git stash create`-style ref (or a content snapshot of touched files) — and the review pane diffs the post-turn tree against **that**. The pane shows only what *this turn* changed, regardless of pre-existing WIP.

**Parallel clobbering.** N concurrent `acceptEdits` agents on one tree would interleave. So:

- **Focused/solo agent:** main working tree, per-turn baseline snapshot as above.
- **Fanout / background / loop editing agents:** each gets its own `git worktree` (`git worktree add`, child cwd set to it). Each DiffReviewer diffs *its own* worktree; the board's `gm`/`gd` (§4.4) reconciles.

```
repo/                      ← main working tree (focused agent; per-turn baseline snapshot)
repo/.avim/wt/agent-3f/    ← fanout member A worktree  → its own git diff
repo/.avim/wt/agent-9c/    ← fanout member B worktree  → its own git diff
```

### 6.4 Obtaining per-edit diffs

The DiffReviewer computes hunks from disk against the per-turn baseline, not from the event stream. Tool-use `old_string`/`new_string` are only a *preview* signal (they drift from what lands — whitespace normalization, failed edits, formatters/hooks). Git is authoritative.

```
turn `result` event  ──►  App marks agent's tree "diff-dirty"
                          + gitignore-aware, debounced fs watcher flips dirty on real writes
        │
        ▼  (EffectRunner, off the UI task)
   git -C <tree> diff --no-color <per-turn-baseline>   ← this turn's changes only
   git -C <tree> status --porcelain=v1                 ← untracked / renames
        │
        ▼
   similar: parse each file's before/after into hunks + inline word diff
        │
        ▼  AppEvent::DiffReady { agent, files: Vec<FileDiff> }
   update() merges into agent.diff, PRESERVING per-hunk Approved/Rejected state by HunkId
```

**Watcher hygiene (must-fix for daily jank).** The `notify` watcher is **gitignore-aware** (skips `target/`, `.git/`, `node_modules/`, anything git-ignored), **debounced ~200–300 ms**, and only re-diffs files git actually reports as changed — otherwise a build or formatter churn triggers constant re-diffs and flicker. Refresh triggers: (1) each turn `result`, (2) a debounced fs event, (3) explicit `:diff`. All git calls run as effects off the UI task.

### 6.5 Hunk model

We diff the baseline blob against the on-disk file with `similar` (not raw `git diff` text) so we own hunk boundaries and get inline word-level changes.

```rust
struct FileDiff {
    path: PathBuf,
    status: FileStatus,        // Modified | Added | Deleted | Renamed
    hunks: Vec<Hunk>,
    lang: Option<Language>,
}

struct Hunk {
    id: HunkId,                // stable within a refresh; also used to re-anchor state across refreshes
    old_range: Range<usize>,
    new_range: Range<usize>,
    lines: Vec<DiffLine>,
    state: HunkState,          // Pending | Approved | Rejected  — AVIM-OWNED, survives re-diff
}

enum DiffLine {
    Context(String),
    Del { spans: Vec<InlineSpan> },
    Ins { spans: Vec<InlineSpan> },
}
```

```rust
let diff = TextDiff::from_lines(&baseline_blob, &disk_text);
for group in diff.grouped_ops(3 /* context lines */) {
    let mut hunk = Hunk::new();
    for op in group {
        for change in diff.iter_inline_changes(&op) {
            match change.tag() {
                ChangeTag::Equal  => hunk.push_context(change.value()),
                ChangeTag::Delete => hunk.push_del(inline_spans(&change)),
                ChangeTag::Insert => hunk.push_ins(inline_spans(&change)),
            }
        }
    }
    file.hunks.push(hunk);
}
```

Diff bodies are syntax-highlighted through the same Highlighter as transcript code blocks (§7).

### 6.6 Diff-view UI

v1 layout is the transcript-beside-diff vsplit.

```
┌ agent:api-refactor ─────────────────┬ diff · 3 files · 5 hunks ──────────┐
│ ▸ you: refactor the auth handler…   │ src/auth.rs        [M]  2 hunks     │
│ ▸ claude: I'll extract the token…   │ ───────────────────────────────    │
│   ⚙ Edit src/auth.rs                 │  @@ -14,7 +14,9 @@ fn verify(…)      │
│   ⚙ Edit src/routes.rs               │  14  fn verify(tok: &str) -> bool { │
│   ✓ done  ($0.06 · 2 turns)          │ -15    let p = parse(tok);          │  ◀ ]c cursor
│                                     │ +15    let p = parse(tok)?;         │
│                                     │ +16    let p = p.validate();        │
│                                     │  17    p.is_valid()                 │
│                                     │  ─────────────────────────── [x]▐   │
├─────────────────────────────────────┴────────────────────────────────────┤
│ NORMAL  api-refactor  sonnet  acceptEdits  hunk 1/5  ✓2 ✗0 ·3            │
└────────────────────────────────────────────────────────────────────────────┘
```

Gutter signs: pending `▐`, `✓` approved (kept), `✗` rejected (reverse-applied). Statusline shows `hunk i/N` and approved/rejected/pending counts. When focus is in the diff pane, motions and operators act on hunks; in the transcript, on messages.

### 6.7 `]c` / `[c` navigation

Ordinary keymap-trie paths, scoped to the diff pane, count-aware:

| Key | Action | Notes |
|---|---|---|
| `]c` | next hunk | wraps to first at end (configurable) |
| `[c` | previous hunk | |
| `]C` / `[C` | last / first hunk | |
| `{count}]c` | jump N hunks forward | count applied at dispatch |
| `zz` | center current hunk | reuses transcript reposition |

Cursor is a `HunkId`, not a line, so a refresh mid-review re-anchors to the nearest surviving hunk instead of jumping to a stale line.

### 6.8 Approve / reject / undo semantics

**Approve and reject are asymmetric, and this is deliberate.**

- **Approve (`s`) is pure aeovim-side bookkeeping** — it sets `HunkState::Approved`. It does **not** stage into git. (Staging would not remove the hunk from a `git diff <baseline>` — the diff shows staged *and* unstaged changes vs the baseline — so "approve = git stage" would either do nothing visible or, worse, get clobbered on the next re-diff.) Approved/Rejected state is **aeovim-owned and preserved across re-diffs by re-anchoring on `HunkId`**. The baseline stays the per-turn snapshot; approve simply marks a hunk reviewed-and-kept (and excludes it from any later bulk revert). Correspondingly, **the diff view is not fully rebuilt from git on every op** — the *hunk geometry* comes from git, but the review state is merged in from aeovim's records.
- **Reject (`x`) is the git operation:** build a one-hunk patch and reverse-apply it.

| Key | Semantics | Implementation |
|---|---|---|
| `s` | **approve/keep** hunk | aeovim-side `HunkState::Approved` (no git write) |
| `x` | **reject** hunk | one-hunk patch → `git apply -R`; whole-file add → `git restore`/delete |
| `S` | approve all in file | mark all hunks Approved |
| `X` | reject all in file | `git restore <file>` / `git checkout -- <file>` |
| `u` | undo last hunk op | re-apply the inverse of the last reject (LIFO stack); for approve, clear the mark |
| `v` then `s`/`x` | visual multi-hunk select | one combined patch for a rejected range; bulk mark for approve |
| `:'<,'>` | ex range over selected hunks | e.g. `:'<,'>DiffReject` |

```rust
fn reject_hunk(wt: &Path, file: &Path, hunk: &Hunk) -> Result<()> {
    let patch = render_unified_patch(file, &[hunk]);   // single-hunk unified diff
    run(git(wt).args(["apply", "-R", "--index", "-"]).stdin(patch))
        .or_else(|_| run(git(wt).args(["apply", "-R", "-"]).stdin(patch)))?; // fallback
    Ok(())
}
```

After a reject, the DiffReviewer re-diffs against the per-turn baseline and rebuilds hunk *geometry*, re-anchoring surviving approve/reject state by `HunkId`. **`u`** maintains a per-file LIFO patch stack; the durable fallback is always `git restore` / repo history / claude's `/rewind`.

**Tell the agent what you reverted (must-do, not "expected desync").** A rejected hunk means the working tree no longer matches what the agent believes it wrote; its next `Edit` would fail on a stale `old_string`, wasting a confused turn. So on reject, aeovim queues a short note to inject on the agent's next turn — "I reverted your change to `src/auth.rs:14-16`; re-read before editing" — so it re-reads instead of failing an exact-match edit. This is cheap (aeovim already holds the reverted patch) and avoids a wasted turn.

### 6.9 Interaction with claude's permission mode

The whole model hinges on the right permission mode via the right mechanism.

- **Exec the binary by absolute path, never the shell** (the alias forces `bypassPermissions`).
- **v1 default: `--permission-mode acceptEdits`.** Edits apply without interactive stalls; review happens after the fact in git.
- **Non-edit tools still gate — this is the v1 blocker to resolve, not defer.** Under `acceptEdits`, `Bash`, `git`, MCP calls, tests, and builds are *not* auto-approved and, headless, **auto-deny** into `result.permission_denials[]`. Since agents run tests/build/git constantly, the "do everything claude can do" loop would silently stall. **v1 ships a concrete default allow-set** via `--allowedTools`, sourced from config so it's tunable — e.g. `Bash(cargo *)`, `Bash(git status:*)`, `Bash(git diff:*)`, `Bash(git log:*)`, `Read`, `Grep`, safe MCP read tools. **And** the denial path is first-class: any denied tool surfaces as a **yellow "approve+retry" board row** (`a` re-issues the turn with that tool allowed), and the control-protocol `can_use_tool` fast keyboard approve/deny POC is pulled forward alongside `interrupt` so an ad-hoc approve exists early. aeovim never ships v1 with only the two bad extremes (coarse `acceptEdits` that denies Bash, or `bypassPermissions` that kills review).

The **`:perm` command** cycles the focused agent's mode. In v1 this restarts/re-resumes with the new `--permission-mode` (with the reload-cost warning of §5.6); the fast-follow wires it to a live `set_permission_mode` control_request once verified.

**Fast-follow manual gate.** When a non-pre-approving mode is active *and* the `initialize` handshake advertised `canUseTool` (§5.6), each `Edit`/`Write` arrives as a `can_use_tool` control_request. aeovim renders the proposed `tool_input` as a preview diff (same hunk widget, no git needed), the user approves/denies the *whole call*, and aeovim replies:

```rust
enum GateDecision { Allow { updated_input: Option<Value> }, Deny { message: String } }
// CLI → avim:  {type:"control_request", request_id, request:{subtype:"can_use_tool", tool_name, input, tool_use_id}}
// avim → CLI:  {type:"control_response", response:{subtype:"success", request_id,
//               response:{behavior:"allow"|"deny", updatedInput?, message?}}}
```

Sub-hunk approval in this mode is *not* natively expressible — it degrades to deny-with-reason and re-prompt. That is exactly why per-hunk fidelity stays in git and the manual gate remains a coarse pre-flight.

### 6.10 DiffReviewer component summary

```
DiffReviewer (per agent)
  owns:   worktree path, per-turn baseline ref, Vec<FileDiff>, hunk cursor (HunkId),
          per-hunk Approved/Rejected state, undo stack, gitignore-aware notify watcher
  inputs: turn `result`, debounced fs events, hunk-motion actions, s/x/S/X/u actions
  effects (off UI task, results fed back as AppEvents):
          git diff <baseline> / git status  → rebuild hunk geometry via `similar`
          git apply -R / git restore → mutate tree, then re-diff; re-anchor state by HunkId
  outputs: FileDiff list + cursor for the Renderer's diff pane;
           approved/rejected/pending counts for statusline & board;
           on reject, a "reverted X" note queued for the agent's next turn
```

The reviewer never blocks the render loop, never trusts tool-use diffs over the tree, isolates each turn's changes from pre-existing WIP, and keeps every reject reversible through git.

---

## 7. Tree-sitter Integration

### 7.1 Scope: what tree-sitter is (and is not) for

The vision line "use treesitter to manage all the tabs" is reframed into what tree-sitter can actually do. Tab/agent/window management is aeovim's own object model — a parser generator has no role there. tree-sitter earns its place on **buffer content**:

| Job | Status in v1 | Consumer |
|---|---|---|
| Syntax-highlight fenced code blocks in transcripts | Staged (behind `syntect`/`two-face`) | `Renderer` transcript view |
| Syntax-highlight git diff hunks | Staged | `DiffReviewer` view |
| Structural in-buffer motions (jump by code node, `%`, fold) | Optional fast-follow | `KeymapEngine` motions |
| Managing tabs/windows/agents | **Never** — aeovim's object model owns this | — |

Decision, stated plainly: **tree-sitter is for highlighting and structural navigation of buffer content, not for tab management.** "Structural navigation of conversation content" means navigation *inside* a focused buffer (by AST node in a code block, or by heading/message via the Markdown grammar), not a replacement for `gt`/`gT`.

### 7.2 Staged rollout: `syntect` first, tree-sitter for pinned languages

```
                       ┌─────────────────────────────────────────┐
   fenced code block   │  Highlighter::highlight(lang, source)     │
   ("```rust\n…")  ───▶ │   lang in PINNED_TS_LANGS ?                │
                       │        │yes                 │no / TS fail  │
                       │        ▼                    ▼              │
                       │   tree-sitter-highlight   syntect+two-face │
                       │        └────────┬───────────┘              │
                       │                 ▼                          │
                       │        Vec<(Range, StyleId)>  →  spans      │
                       └─────────────────────────────────────────┘
```

- **Stage 1 (always present):** `syntect` 5.3 + `two-face` 0.5 covers every language via bundled `.sublime-syntax`, pure-Rust, no C toolchain. The floor and permanent fallback.
- **Stage 2 (pinned core languages):** `tree-sitter` 0.26.x + `tree-sitter-highlight` for the handful of languages the author lives in. tree-sitter handles nesting, injections, and produces the parse tree §7.6's structural motions need — which syntect cannot.

If a fenced block's info string maps to a pinned grammar that loaded cleanly, use tree-sitter; otherwise fall through to syntect. The fallback is silent and per-language.

### 7.3 Grammar loading and `HighlightConfiguration`

Each pinned language builds one `HighlightConfiguration` once, at startup. **Each `build_*()` is written against its own crate's surface** — grammar crates differ in exported constants and language accessors, so there is no single macro for all six.

```rust
use tree_sitter_highlight::{HighlightConfiguration, Highlighter};

const HL_NAMES: &[&str] = &[
    "attribute", "comment", "constant", "constant.builtin", "constructor",
    "function", "function.builtin", "function.macro", "keyword", "label",
    "number", "operator", "property", "punctuation.bracket",
    "punctuation.delimiter", "string", "string.escape", "string.special",
    "tag", "type", "type.builtin", "variable", "variable.builtin",
    "variable.parameter",
];

struct LangEntry { config: HighlightConfiguration }

fn build_rust() -> anyhow::Result<LangEntry> {
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    // Guard against ABI skew before parsing. NOTE: confirm the accessor name on 0.26 —
    // older cores expose `version()`, newer `abi_version()`; guard accordingly.
    let abi = language.abi_version();
    anyhow::ensure!(
        tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION <= abi
            && abi <= tree_sitter::LANGUAGE_VERSION,
        "rust grammar ABI {abi} outside supported range"
    );
    let mut config = HighlightConfiguration::new(
        language,
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY, // exact const name is per-crate (see below)
        tree_sitter_rust::INJECTIONS_QUERY, // Rust HAS injections; others may not — pass "" then
        "",                                 // locals query optional
    )?;
    config.configure(HL_NAMES);
    Ok(LangEntry { config })
}
```

**Per-crate query surface (compile-correctness).** Not every pinned crate exports an `INJECTIONS_QUERY` (e.g. `tree-sitter-json` has none), and some export `HIGHLIGHT_QUERY` (singular) rather than `HIGHLIGHTS_QUERY`. Each `build_*()` passes `""` for a missing injection/locals query and references the **exact** constant its crate provides — hardcoding `INJECTIONS_QUERY` in a shared template is a compile error for grammars lacking it.

`configure(HL_NAMES)` resolves each grammar's capture (e.g. `@function.method`) to the **longest matching prefix** in `HL_NAMES` (`function`), folding all grammars into one small style vocabulary.

Pinned grammar set for v1 — small, chosen by what the author writes, each ABI-gated:

| Language | Grammar crate | Version | Fallback if load fails |
|---|---|---|---|
| Rust | `tree-sitter-rust` | 0.24.2 | syntect |
| TypeScript / JS | `tree-sitter-typescript` | 0.23.2 | syntect |
| Python | `tree-sitter-python` | 0.25.0 | syntect |
| Bash / shell | `tree-sitter-bash` | 0.25.1 | syntect |
| JSON | `tree-sitter-json` | 0.24.8 | syntect *(no injections query — pass "")* |
| Markdown | `tree-sitter-md` | 0.5.3 | syntect (+ message structure, §7.6) |

Everything else is syntect-only in v1; adding a grammar is a registry entry plus an ABI check.

### 7.4 Mapping captures to ratatui styles

```rust
use tree_sitter_highlight::HighlightEvent;
use ratatui::style::{Style, Color, Modifier};

struct Theme { by_capture: [Style; 24], plain: Style }  // parallel to HL_NAMES; TOML-loaded

fn highlight_block(hl: &mut Highlighter, e: &LangEntry, src: &[u8], theme: &Theme)
    -> Vec<(std::ops::Range<usize>, Style)>
{
    let mut out = Vec::new();
    let mut stack: Vec<Style> = vec![theme.plain];
    let events = hl.highlight(&e.config, src, None, |_| None).unwrap();
    for ev in events {
        match ev.unwrap() {
            HighlightEvent::HighlightStart(h) => stack.push(theme.by_capture[h.0]),
            HighlightEvent::HighlightEnd     => { stack.pop(); }
            HighlightEvent::Source { start, end } =>
                out.push((start..end, *stack.last().unwrap())),
        }
    }
    out
}
```

The injection callback (`|_| None`) can later return a nested `HighlightConfiguration` for injected languages; v1 leaves injections off since top-level fence detection handles the common case.

| `HL_NAMES` entry | ratatui `Style` |
|---|---|
| `keyword` | `fg(Color::Magenta)` |
| `function`, `function.macro` | `fg(Color::Blue)` |
| `type`, `type.builtin` | `fg(Color::Yellow)` |
| `string`, `string.special` | `fg(Color::Green)` |
| `number`, `constant` | `fg(Color::Cyan)` |
| `comment` | `fg(Color::DarkGray).add_modifier(Modifier::ITALIC)` |
| `variable.parameter` | `fg(Color::Rgb(0xE0,0xAF,0x68))` |
| `punctuation.*`, `operator` | `plain` |

### 7.5 Span caching: only the streaming message re-highlights

Highlighting is memoized at the **message** granularity keyed by content hash — the single settled-message cache of §3.5. Completed messages hash-stable, so their spans compute once; only the in-flight assistant message (and only its trailing code block) rebuilds, on dirty ticks. Reconciled with §3.4: **one cache, LRU-bounded, keyed by final content hash; the streaming message renders on a non-cached fast path and is inserted exactly once when finalized.**

```rust
struct SpanCache { by_hash: LruCache<u64, Arc<Vec<Line<'static>>>> } // bounded; SETTLED only

fn settled_message_lines(cache: &mut SpanCache, msg: &Message, hl: &Highlighter, ..)
    -> Arc<Vec<Line<'static>>>
{
    let h = msg.content_hash;
    if let Some(l) = cache.get(&h) { return l.clone(); }
    let arc = Arc::new(render_message(msg, hl, ..));
    cache.put(h, arc.clone());
    arc
}
// The streaming message uses a separate render_streaming() path each dirty tick — NO cache insert.
```

Two cost controls: **(1) off-thread for large finalized blocks** — when `assistant`/`result` finalizes a big block, the `Highlighter` runs as an `EffectRunner` task (blocking pool) and the spans come back as an app event that fills the cache; **(2) viewport virtualization** — only visible messages are highlighted; off-screen unfocused agents highlight lazily when focused.

Streaming subtlety: a growing block may be an unterminated fence. The scanner treats "open fence to end-of-buffer" as provisional and highlights optimistically; when the closing fence arrives the hash changes and it re-highlights once. tree-sitter parses partial source gracefully (produces `ERROR` nodes rather than failing), which is why it tolerates mid-stream code.

### 7.6 Structural navigation of buffer content (fast-follow)

Because tree-sitter already produced a parse `Tree` per highlighted block, motions inside the focused buffer come free:

| Motion | Meaning | Implementation |
|---|---|---|
| `]f` / `[f` | next/prev top-level item in the focused code block | walk named children of the root |
| `%` | jump to matching bracket/delimiter | `node.parent()` of the bracket |
| `zc` / `zo` (later) | fold/unfold a code node | node byte range → collapse |
| `]]` / `[[` | next/prev message or Markdown heading | `tree-sitter-md` over the transcript |

```rust
fn next_item(tree: &tree_sitter::Tree, cursor_byte: usize) -> Option<usize> {
    let root = tree.root_node();
    (0..root.named_child_count())
        .filter_map(|i| root.named_child(i))
        .filter(|n| matches!(n.kind(),
            "function_item" | "struct_item" | "impl_item"
            | "function_definition" | "class_definition"))
        .map(|n| n.start_byte())
        .find(|&b| b > cursor_byte)
}
```

These are ordinary trie paths; the work is mapping the resolved motion to a tree query. Scoped fast-follow because v1's must-haves are the highlighter and diff review.

### 7.7 Grammar build and linking considerations

- **C compiler at build time.** Every `tree-sitter-*` grammar compiles generated `parser.c`/`scanner.c` via the `cc` crate; the target macOS host needs Xcode CLT. Build-host requirement only, not distribution.
- **ABI version range, not equality.** Core 0.26 accepts a *range* of language ABIs. The pinned grammars declare core deps spanning ~0.23–0.25, fine as long as each generated parser's ABI falls inside 0.26's window. The `ensure!` (§7.3) turns an out-of-range grammar into a clean startup fallback to syntect.
- **Query-string compatibility.** `HighlightConfiguration::new` returns `Err` on a bad query — caught and downgraded per-language, never fatal.
- **API drift across grammar crates.** Language export (`fn language()` vs `const LANGUAGE`) and query constant names vary; each `build_*()` targets its crate's surface. Pin exact versions so `cargo update` can't swap the export shape.
- **Pin + verify each grammar individually** (open decision): confirm each loads under 0.26 on the actual build; budget for at least one needing a patched/forked version. syntect stays wired as the guaranteed floor.

Net: tree-sitter is adopted narrowly and defensively — small pinned set, ABI-checked, silently replaceable by syntect per-language.

---

## 8. State, Persistence & Configuration

avim keeps its own state deliberately thin. Claude owns the authoritative transcript on disk; avim owns the *workspace* (which agents exist, layout, scheduled loops/jobs) plus a normalized transcript cache so it never parses Claude's internal JSONL. This split lets avim restart, reattach, and survive a Claude version bump that changes the `.jsonl` shape. **For a single-author daily driver, §8 is deliberately trimmed** — the enterprise-grade bits (4-layer config precedence, schema-migration tooling, lock files, retention sweeps) are deferred until actually needed; the genuinely valuable bits (workspace restore, self-minted session-id cache, atomic writes) ship in v1.

### 8.1 Two persistence layers — who owns what

| Concern | Owner | Location | Format | Stability |
|---|---|---|---|---|
| Conversation memory (for `--resume`) | **Claude Code** | `~/.claude/projects/<cwd-hash>/<session-id>.jsonl` | Claude-internal JSONL | Unstable — avim never parses it |
| Session identity | **avim** (self-minted UUID) | avim workspace state | UUID | Stable; avim's key into Claude's store |
| Rendered transcript / normalized events | **avim** | `sessions/<session-id>.ndjson` | avim `AgentEvent` NDJSON | avim-controlled |
| Tabs, window tree, focus, agent index | **avim** | `workspaces/<ws-hash>/state.json` | avim schema | avim-controlled |
| Loops, fanout groups, budgets | **avim** | `workspaces/<ws-hash>/jobs.json` | avim schema | avim-controlled |
| Keymap / theme / defaults / backends | **avim** | `config.toml`, `keymap.toml` | TOML | avim-controlled |

Because avim mints the `--session-id` and keeps its own transcript cache, **reattach never depends on Claude's storage format or cwd-hash derivation.** avim resumes by explicit id and renders from its own NDJSON. Claude's `.jsonl` is opaque, touched read-only only when *importing* pre-existing sessions into a `:resume` picker.

### 8.2 On-disk layout

Paths resolve XDG-first, falling back to fixed `~/.config/avim`, `~/.local/share/avim`, `~/.local/state/avim` even on macOS (the author expects neovim-style paths, not `~/Library/Application Support`). `dirs 6` supplies fallbacks; explicit env vars win.

```
~/.config/aeovim/                    # config (hand-edited)
├── config.toml
├── keymap.toml
└── themes/default.toml

~/.local/share/aeovim/               # state (machine-owned, restorable)
├── workspaces/<ws-hash>/
│   ├── state.json                 # tabs, window tree, agent index, focus
│   ├── jobs.json                  # loops, fanout groups, budgets
│   └── sessions/
│       ├── <session-id>.ndjson    # normalized transcript cache
│       └── <session-id>.raw.jsonl # optional raw stream dump (debug)
└── worktrees/                     # aeovim-created git worktrees for parallel edits

~/.local/state/aeovim/log/aeovim.log   # tracing rolling file
```

### 8.3 Workspace / project concept (per-cwd agents)

A **workspace** is "these agents belong to this project," resolved once at launch:

```rust
fn resolve_workspace(launch_cwd: &Path, cfg: &Config) -> Workspace {
    let root = if cfg.project.detect_git_root {
        git_toplevel(launch_cwd).unwrap_or_else(|| launch_cwd.to_owned())
    } else {
        launch_cwd.to_owned()
    };
    let hash = blake3_hex(root.canonicalize().unwrap());
    Workspace { root, hash, dir: data_dir().join("workspaces").join(&hash) }
}
```

Launching `avim` inside a repo restores that repo's tabs, agents, and loops; launching elsewhere yields an isolated workspace.

- Each agent stores its own `cwd` and (if isolated) `worktree` in `state.json`; a spawned child launches with that recorded cwd.
- Parallel/fanout agents each get a worktree under `worktrees/<ws-hash>/<agent-id>`; the board's `gm`/`gd` (§4.4) reconciles.
- `[project].auto_add_dir` injects extra `--add-dir` paths for multi-root repos.

### 8.4 Session persistence & restore

**Minting & dual-write.** Every agent is spawned with `--session-id <uuid>` avim generates. The reader appends normalized `AgentEvent`s to `sessions/<uuid>.ndjson` before they reach the reducer; that NDJSON is avim's transcript of record; Claude's `.jsonl` is the resume substrate.

**Restore is lazy (detached-first).** Reattaching every agent on launch would spawn N heavyweight children and burn quota/RAM. So on restart:

```
launch aeovim
   ├─ load config.toml + keymap.toml
   ├─ resolve workspace → load state.json + jobs.json
   ├─ for each Agent: status = Detached
   │     transcript ← replay sessions/<uuid>.ndjson   (instant, no child)
   ├─ rebuild tabs / window tree / focus
   ├─ loops flagged active → recreate scheduler tasks, PAUSED
   └─ render
        └─ on first interaction with a Detached agent (focus+compose, send turn, :resume):
              spawn `claude -p --resume <uuid> …` with recorded cwd; status → Live
```

This resolves the idle-session question: **detached is the resting state, live is earned on interaction.** Reconnect latency is one cold `--resume` spawn, paid only on the agent you touch. `[general].warm_focused = true` eagerly re-spawns just the focused tab for zero-latency first keystroke.

**Loops restart paused.** A loop silently resuming after a crash could spend money unattended, so `jobs.json` loops come back **paused** (yellow, with run-count and one-key resume). In-flight turn/fanout work is transient and not restored.

**Atomic, debounced writes.** State is written on a dirty flag, debounced (~500 ms) off the UI task, via temp-file + `rename`.

```rust
fn save_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path) // atomic on same filesystem
}

#[derive(Serialize, Deserialize)]
struct WorkspaceState {
    schema_version: u32,          // present now; migration tooling deferred until it first bumps
    tabs: Vec<TabState>,
    windows: WindowArena,
    agents: Vec<AgentState>,      // { agent_id, session_id, backend, model, permission_mode,
                                  //   cwd, worktree, name, status_snapshot, cost_usd }
    focus: (TabId, WinId),
    board_open: bool,
}
```

`schema_version` is stamped now for forward-compat, but migration tooling is deferred until the first bump. Autosave-on-dirty is the default; a `SIGTERM` (handled by the same reaper that killpg's children) flushes cleanly.

### 8.5 Configuration file (`config.toml`)

**v1 uses two files (config + keymap), not a 4-layer precedence.** The layering is: built-in defaults → `~/.config/avim/config.toml` → CLI flags. (A per-project `.avim/config.toml` overlay is a deferred nicety, not v1.) Loaded into typed structs (serde + `toml 1.1`); unknown keys warn but don't abort. `:reload` re-reads; a `notify 8.2` watch offers hot-reload of keymap/theme/defaults.

```toml
[general]
default_model            = "sonnet"
default_permission_mode  = "acceptEdits"
default_effort           = "medium"
spawn_semaphore          = 6               # max concurrent LIVE children
loop_cost_cap_usd        = 2.0             # aeovim SOFT cap — the budget authority for loops
max_budget_usd           = 20.0            # coarse per-child HARD backstop, ABOVE the soft cap
tick_fps                 = 40
timeoutlen_ms            = 1000
restore_workspace        = true
warm_focused             = true
self_paced_floor         = "30s"           # minimum inter-iteration delay for self-paced loops

[paths]
claude_binary  = "/opt/homebrew/bin/claude"  # ABSOLUTE — never the shell alias
worktree_root  = "~/.local/share/aeovim/worktrees"

[permissions]
# Default allow-set so headless non-edit tools don't auto-deny (§6.9). Tunable.
allowed_tools = [
  "Read", "Grep", "Glob",
  "Bash(cargo *)", "Bash(git status:*)", "Bash(git diff:*)", "Bash(git log:*)",
]

[project]
detect_git_root = true
auto_add_dir    = []

[loop]
default_interval  = "5m"
resume_paused     = true

[logging]
level     = "info"
file      = true
raw_dump  = false
max_files = 5

[theme]
name = "default"

# Backend registry — drives the AgentBackend seam. Only claude-code ships in v1.
[[backend]]
id      = "claude-code"
kind    = "claude-cli"
default = true
```

**Budget authority (resolves the double-govern hazard).** For a loop running many turns on one persistent child, avim's accumulated `total_cost_usd` summation is the enforcement authority (cancel via token when `loop_cost_cap_usd` trips). `--max-budget-usd` is set on the child only as a *coarse hard backstop well above* the soft cap; a CLI budget abort surfaces as `result:error` → red job, and avim's soft cap should trip first.

### 8.6 Keymap file (`keymap.toml`)

Bindings live in a dedicated file, reloadable independently. Keys use **crokey 1.4** notation; values name an `Action` variant. avim compiles these into the per-mode trie (§2.6); `g t` or `] c` are just multi-edge paths.

```toml
[normal]
"g t"        = "next_tab"          # cycle agents
"g T"        = "prev_tab"
"g a"        = "agent_picker"
"] c"        = "next_hunk"
"[ c"        = "prev_hunk"
"<c-w> v"    = "vsplit"
"<c-w> s"    = "split"
"<space> n"  = "new_agent"
"<space> f"  = "fanout"
"<space> l"  = "start_loop"
"<space> s"  = "skill_palette"
"<space> t"  = "task_board"
"<space> p"  = "cycle_permission_mode"
"<space> q"  = "detach_agent"      # hide, keep running
"<space> K"  = "kill_agent"        # reap child group

[insert]
"alt-<cr>"   = "send_prompt"       # guaranteed fallback; kitty <c-cr> preferred when available

[visual]
"y"          = "yank"

[diff]              # active only when a diff window is focused
"s"          = "approve_hunk"      # aeovim-side keep-mark
"x"          = "reject_hunk"       # git apply -R / restore
"u"          = "undo_hunk_op"
```

```rust
for (chord, action_name) in table {
    let keys: Vec<KeyEvent> = crokey::parse_seq(&chord)?;
    let action: Action = Action::from_name(&action_name)?;  // errs on typo
    keymap.insert(mode, keys, action);
}
```

Unknown action names are a load error surfaced in the statusline (not a crash); the previous good keymap stays live. The file merges onto built-in defaults; setting an action to `"noop"` unbinds it. (Note: bare `q` is intentionally not defaulted to a destructive action — §2.4.)

### 8.7 Logging

The TUI owns the terminal, so **nothing logs to stdout/stderr.** `tracing` + `tracing-appender` write a rolling file at `~/.local/state/aeovim/log/aeovim.log`, filtered by `[logging].level`. A panic hook first restores the terminal, *then* logs the backtrace.

| Channel | Level | Content |
|---|---|---|
| App lifecycle | info | Launch, workspace resolve, spawn/reattach, shutdown |
| Effects | debug | Spawn cmds (redacted), git diff/apply, highlight jobs |
| Backend I/O | trace | Normalized `AgentEvent`s, control requests |
| Raw stream | (opt) | Verbatim `stream-json` → `sessions/<id>.raw.jsonl` when `raw_dump = true` |
| Errors | warn/error | Deser fallbacks (`Unknown` events, kept raw + logged), rate limits, budget hits, git failures |

`:messages` tails the structured log; `:redir <id>` dumps raw stream-json for a session — the debugging seam for when Claude's event shape drifts.

### 8.8 Persistence notes & deferred decisions

- **Version gating.** The `claude_code_version` from `system/init` is stored per session; a mismatch on reattach logs `warn` and, if the schema looks incompatible, avim degrades to read-only transcript display rather than sending turns into a stale session.
- **Single-instance assumption (v1).** avim assumes one instance per workspace. A lock file (`workspaces/<ws-hash>/.lock`) is a *deferred* nicety; for v1 a second launch is simply the author's responsibility. Concurrent hand-run `claude` in the same repo is tolerated (separate session-ids) but not merged unless imported via `:resume`.
- **Retention.** avim's own `sessions/*.ndjson` retention sweep is deferred; Claude's `cleanupPeriodDays` governs its `.jsonl`, so an expired Claude session may become non-resumable while avim still holds the rendered transcript (read-only).

---

## 9. Open Questions

Unresolved decisions, grouped by area. Items marked **(spike)** should be answered in the walking-skeleton spike before the milestone that depends on them.

**Claude CLI wire facts to verify (spike).**
- Exact JSON field keys and casing of the `system/init` line (`permissionMode` camelCase confirmed in the wild; capture a fixture and annotate/round-trip-test every field). **(spike)**
- The correct delta-correlation key for coalescing: almost certainly `(message_start id, content_block index)`, not the envelope `uuid` — confirm on a multi-block streaming turn. **(spike)**
- Canonical permission-mode names on the pinned version (`manual` vs `default` alias; whether `auto` is real; which mode leaves edits un-pre-approved). **(spike)**
- The `control_request`/`control_response` envelope and whether the `initialize` handshake advertising `canUseTool` is what routes per-call gating — gates both the manual gate and live `set_permission_mode`/`interrupt`. **(spike)**
- Live `set_permission_mode` / model swap on a running child (binary contains a "not supported in this context" string).
- `claude -w/--worktree` behavior for fanout isolation; fall back to avim-managed `git worktree add` if unreliable.
- `claude agents --json` shape and `--bg` semantics for background agents (version-fragile; deferred to fast-follow).
- Model alias set (`fable|opus|sonnet`) vs full ids.

**Interaction / product decisions.**
- The exact default `--allowedTools` set (§6.9) — needs a first pass from the author's real workflow, tunable in config.
- Fan-out merge/discard conflict handling beyond "bail to a message" (§4.4) — how much conflict UX to build.
- Self-paced loop completion sentinel format and default floor delay (§4.5).
- Whether `:model`/`:perm` should ever batch onto the next turn's flags vs always restart when live control is unavailable (§5.6).

**Engineering knobs.**
- Spawn-semaphore default cap (config default 6; validate against real memory footprint of N Node children + MCP servers).
- Per-turn baseline mechanism for the solo agent: `git stash create` ref vs content snapshot of touched files (§6.3).
- Watchdog silence threshold for flagging stalled children.

**Deferred (post-v1) scope.**
- Arbitrary N-way window trees (v1 ships tabs + one two-pane vsplit).
- Pre-apply manual gate (control-protocol `can_use_tool`) — after the handshake is proven.
- Live `set_permission_mode`/model control — after the control POC.
- Background/detached agents via `claude --bg` + polling.
- tree-sitter structural motions (`]f`/`[f`/`%`/folding).
- Per-project `.aeovim/config.toml` overlay, schema-migration tooling, single-instance lock file, session retention sweep.
- Whether to mirror `--session-id` into `CLAUDE_CONFIG_DIR` isolation for sandboxed test workspaces.
- Per-grammar ABI verification under tree-sitter 0.26 on the actual build host; budget for one grammar needing a fork.