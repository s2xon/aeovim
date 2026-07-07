# aeovim — Claude Code integration research & plan

aeovim already talks to Claude Code over `--output-format stream-json`; it just discards most of what comes back. The goal here is straightforward: get the best possible Claude Code functionality inside aeovim — rich tool rendering, diffs, thinking, todos, cost, real permission modes, and (eventually) in-TUI approval, interrupt, and steering — while carving a clean `AgentBackend` seam so Codex, and any MCP-configured Desktop/other agent, can slot in later without forking the event model.

Two facts from the tree (`src/agent.rs`, `src/protocol.rs`, `src/app.rs`, `src/ui.rs`, `src/store.rs`) shape everything:

- **We fetch a lot and throw it away.** `spawn_turn` runs `--output-format stream-json --verbose --include-partial-messages`, so tool inputs, `tool_result` blocks, `thinking` deltas, `TodoWrite` payloads, subagent activity, and per-turn `usage` are already on the wire. `protocol.rs` collapses them to `Ignore` or `ToolUse(name)`. The single biggest win is parsing what we already receive. (Note: `protocol.rs` *already* parses `slash_commands` out of the `Init` event at lines 50–63, so the palette seed is half-built — the plan's earlier claim that Init keeps "only session_id/model" was wrong.)
- **Fire-and-forget forbids interactivity.** `spawn_turn` sets `stdin(Stdio::null())` and spawns one child per turn. That structurally rules out interactive permission approval, `AskUserQuestion`, MCP elicitation, `Esc`-interrupt, and mid-turn steering. Those require a persistent child on the bidirectional control protocol — a deliberate milestone, not a tweak.

---

## 1. Capability catalog

| Capability | How to drive it | Inherited vs build | How aeovim surfaces it | Priority |
|---|---|---|---|---|
| Rich tool events (inputs+results) | Already emitted on `stream-json`; parse `assistant.message.content[].tool_use.input` and `type:"user"` → `tool_result` blocks | build (parse) | New `AgentEvent::ToolCall{id,name,input}` / `ToolResult{id,ok,text}`; new `Entry::ToolCall/ToolResult`; render in `ui.rs` | must |
| Edit/Write diffs | From `tool_use.input` for `Edit`(old/new_string), `Write`(content), `MultiEdit` | build (render) | Colored diff block in transcript pane | must |
| Bash command + output | `tool_use.input.command`; `tool_result` for stdout | build (render) | Command line + collapsible output under `Entry::ToolCall` | must |
| Extended thinking | `content_block_delta.delta.type == "thinking_delta"`; `content_block_start` type `thinking` | build (parse) | `AgentEvent::ThinkingDelta`; dim/italic foldable `Entry::Thinking` | high |
| TodoWrite plan | `tool_use.name=="TodoWrite"`, `.input.todos[]` | build (parse+render) | Live todo panel per chat | high |
| Cost / usage | `result.total_cost_usd` (have it) + `result.usage.{input,output,cache_read...}` | build (parse) | Statusline `$x.xx · N in / M out` (see gap on resume semantics, §4) | high |
| Permission mode + plan mode | `--permission-mode default\|acceptEdits\|plan` (a `TurnSpec` field, but `spawn_for` hardcodes `"acceptEdits"` at app.rs:1372) | partial | Per-chat `permission_mode`; statusline badge; `Shift-Tab` cycles (aeovim-side re-spawn, not claude's own Shift-Tab) | must |
| Interactive approval (canUseTool) | Persistent child; approvals arrive as control-protocol `canUseTool` requests on the same pipe | build (rewrite) | `Mode::Approve` modal: `y`/`n`/`a` (always-allow tool) | must (P1) |
| AskUserQuestion | Persistent child; structured multi-choice tool, selection returned over control channel | build (rewrite) | Same `Mode::Approve`-style modal; return choice | must (P1) |
| MCP elicitation | Persistent child; register an `Elicitation` hook for server-requested mid-task input | build (rewrite) | Modal; without it, elicit-capable connectors hang a headless turn | high (P1) |
| Interrupt / steer | Persistent child: write control `interrupt`; write a `user` message mid-turn | build (rewrite) | `Esc` while `in_flight` → interrupt; `i` mid-turn → steer | high (P1) |
| Slash/skill palette | `slash_commands` already parsed from `Init`; scan `.claude/commands/*.md`, `.claude/skills/*/SKILL.md` | inherited (custom cmds are prompt text) | Fuzzy `Mode::Picker` seeded from Init; commit sends `/command args` as turn text — **custom commands only** (see §A caveat) | high |
| Built-in interactive commands | `/context`, `/memory`, `/model`, `/rewind` are **not** reliable prompt text | build (native) | Build native equivalents from parsed data / filesystem / git, not by sending the slash string | high |
| MCP servers/tools | `.mcp.json`/`~/.claude.json`; `--mcp-config` per space | inherited **+ auth work** | Render `mcp__*` distinctly; `:mcp` shells `claude mcp list/add/login`; detect auth method | high |
| `@`-resource / `/mcp__` prompts | `@server:proto://path` in prompt; `/mcp__srv__prompt args` as turn text | inherited | `@` popup (reuse file-picker) + palette entries | high |
| Image / PDF input | Claude ingests images/PDFs from paths | inherited | `@file`-attach path into the prompt | nice |
| Subagents (Task) | Auto or `--agents '<json>'`; surfaces as `tool_use name=="Task"` + `tool_result` | partial (unverified) | Nested/tree render **only if** internals actually stream (validate first, §E) | high |
| Background agents | `--bg`; poll `claude agents` (`--json` flag unverified) | inherited | Agents pane; `:agents` | high |
| Session fork / name / resume | `--fork-session` (+ `--resume`), `--name`, `--continue` | inherited | `:fork` branches into new pane; resume picker | high |
| Memory / CLAUDE.md | Direct file editing | build | `:memory` opens `CLAUDE.md`/`MEMORY.md` in a buffer | high |
| Compact | `/compact` (plausible headless semantics — verify against binary) | partial | `:compact`; auto-suggest at ~70% window | nice |
| Checkpoints / rewind | Git-snapshot per turn (primary); `/rewind` unverified headless | build | `leader u`; snapshot layer is the primary, not fallback | nice |
| Model + effort | `--model` (have `model_cli`); `--effort low..max` | inherited | Existing model `Mode::Picker` + effort sub-picker | high |
| Web search / fetch | Auto; `tool_use name in {WebSearch,WebFetch}` | inherited | Results block; respect `WebFetch(domain:*)` rules | nice |
| Structured output | `--json-schema '<schema>'` | inherited | `:structured` turn; parse `result` → table | nice |
| Hook events (observe + gate) | `--include-hook-events`; PreToolUse hooks can **deny/gate** statically today | inherited | Debug pane; **also** the P0 safety story (static policy, no persistent child) | high |
| Auth detection | exit code / stderr; `claude doctor` / `claude /status` | partial | Actionable hint (`run: claude auth`) on spawn failure | must |

---

## 2. Integration roadmap

### P0 — ship now (pure wins, no backend rewrite)

**1. Expand the parser — `protocol.rs`.** Highest-leverage change, one file. Grow `AgentEvent`:
```rust
ToolCall  { id: String, name: String, input: serde_json::Value },
ToolResult{ id: String, ok: bool, text: String },
ThinkingDelta(String),
Todos(Vec<Todo>),                                  // from TodoWrite input
Usage { input: u64, output: u64, cache_read: u64 },
```
- `assistant` branch: iterate `content[]`, emit a `ToolCall` per `tool_use` block carrying `.id`, `.name`, `.input` (currently ~line 93–96 drops `.input`).
- Add a `"user"` top-level branch: `message.content[]` with `type=="tool_result"` → `ToolResult{ tool_use_id, is_error, content }`.
- `stream_event`: handle `delta.type=="thinking_delta"` alongside `text_delta` (~line 66–72).
- `result`: also read `usage.*` (~line 109–113 takes only cost).

**2. Render tools + diffs — `app.rs` `Entry`/`handle_agent`, `ui.rs`.** Replace `Entry::Tool(String)` with:
```rust
ToolCall  { key: u64, name: String, summary: String, body: ToolBody }, // ToolBody::{Diff, Bash, Generic}
ToolResult{ ok: bool, text: String, folded: bool },
Thinking(String),
```
In `handle_agent` (~line 402) match on `name`: `Edit/Write/MultiEdit` → `ToolBody::Diff` from `input`; `Bash` → `ToolBody::Bash{cmd}`. In `ui.rs` (~line 372, currently `⚙ {n}`) render diffs with add/del coloring via `theme.rs`, Bash as `$ cmd` + folded output, thinking dim/italic. Add a `za`-style fold toggle on the focused entry.

**3. Real permission/plan mode — `app.rs` + `agent.rs`.** `spawn_for` (app.rs:1372) hardcodes `permission_mode: "acceptEdits"`. Add `permission_mode: PermMode` to `Chat` (default `AcceptEdits` to preserve the current fast path; `self.dangerous` still maps to `--dangerously-skip-permissions`). Bind **`Shift-Tab`** in `Mode::Normal` to cycle `plan → acceptEdits → default` — this is aeovim re-spawning with a new `--permission-mode`, **not** driving claude's in-REPL Shift-Tab; make that explicit so nobody expects REPL behavior. Statusline badge next to the mode indicator. In `plan`, render the returned plan as `Entry::Plan`.

**4. Slash/skill palette — `app.rs`.** `Init`'s `slash_commands` is already parsed; add a `.claude/commands/*.md` + `.claude/skills/*/SKILL.md` scan (frontmatter for descriptions) into `App`. Reuse `Mode::Picker` for a `/`-triggered fuzzy palette. **Only custom commands/skills** commit as `/command args` through `send_prompt` — they are genuine prompt expansions. Built-in interactive commands (`/context`, `/memory`, `/model`, `/rewind`) are **not** reliable as prompt text (see §A); route those to native handlers, not the palette-as-prompt path.

**5. Cost + usage in statusline — `app.rs`/`ui.rs`.** `Chat.cost` exists; add `last_usage`, render `$x.xxx · Nk in / Mk out` in the focused footer. Before wiring: confirm whether `total_cost_usd`/`usage` on `--resume` are per-turn or cumulative (§G) so you sum vs read correctly.

**6. Hooks as a today policy layer — `agent.rs`.** Turn on `--include-hook-events` for a debug/log pane, and note that PreToolUse hooks can **deny/gate** tool calls in the current per-turn model — a static safety story that ships before P1's interactive approval lands.

**7. Auth/failure surfacing — `agent.rs`.** `TurnEnded.error` already carries stderr (~line 113). Detect auth/login strings and render `Entry::Note("run: claude auth")` instead of a raw dump.

### P1 — persistent child + interactive control (the deliberate rewrite)

**8. Persistent-child backend — `agent.rs`.** Replace one-child-per-turn with one long-lived child per `Chat`: `claude --input-format stream-json --output-format stream-json --verbose --include-partial-messages`, **stdin held open**. Turns become JSON lines to stdin; keep resuming the same process. Store the `tokio` `ChildStdin` writer in `Chat` behind an `Arc<Mutex>`/channel. Load-bearing milestone gating 9–12.

**9. In-TUI approval (canUseTool) — `agent.rs` + `app.rs`.** With the control channel, drop `--dangerously-skip-permissions` for non-YOLO chats. Approvals arrive as **control-protocol `canUseTool` requests on the same stdin/stdout pipe** — this is the SDK path; do **not** assume `--permission-prompt-tool` is needed (it historically expects an MCP tool name like `mcp__approval__prompt`, not a magic `stdio` value). Pin the exact mechanism to the installed binary before building. Incoming request → `Msg::PermissionRequest{chat, tool, input}` → `Mode::Approve` modal (reuse the `Mode::Confirm` pattern ~app.rs:543): `y` allow once / `n` deny / `a` always-allow-this-tool (append to `--allowedTools`). Per-space **YOLO toggle** (`leader !`) reverts to `--dangerously-skip-permissions`.

**10. AskUserQuestion + MCP elicitation — `agent.rs` + `app.rs`.** Same channel, same `Mode::Approve`-style modal. `AskUserQuestion` returns the user's structured choice; register an `Elicitation` hook so server-requested mid-task input doesn't hang the turn. These are core "Claude interviews the user" functionality and have the identical persistent-child requirement as approval — do not defer them behind approval.

**11. Interrupt + steer — `app.rs` keys.** `Esc` in `Mode::Normal` while `chat.in_flight` → control `interrupt`. `i` while in-flight → write a `user` message into open stdin. Route `Msg::Pipe` / `$AEOVIM_PIPE` (`inject_pipe`, app.rs:1407) through the same stdin writer instead of spawning a fresh child — injection into the in-flight turn is exactly the wanted semantics.

**12. Subagent / background panes.** **First validate** whether a subagent's internal turns stream to the parent stdout in headless mode — they run in a separate context and may only surface as one `Task` `tool_use` + final `tool_result` (§E). If they stream, do nested/tree render or an ephemeral Space pane (1–4 panes supported). Add `:agents` shelling background-agent listing (confirm the `--json` flag exists).

**13. Session fork / resume — `store.rs` + `app.rs`.** `:fork` spawns `--fork-session` into a new pane, tracking parent in `PersistChat` (`parent: Option<String>`). Resume picker (`Mode::Picker`) lists recent sessions including those started outside aeovim.

**14. Native context / memory / compact — `app.rs`.** `:context` renders a token breakdown built from the `usage.*` fields (P0-5) — **not** by sending `/context`. `:memory` opens `CLAUDE.md`/`MEMORY.md` in an editor buffer. `:compact` sends `/compact` **only after** verifying its headless semantics against the installed binary; auto-suggest at ~70% of the model window.

### P2 — breadth + second backend

**15.** `AgentBackend` trait + `CodexBackend` (§3). **16.** MCP management view: `:mcp add/list/remove/login`, `@`-resource popup, and the auth handling in §D. **17.** Checkpoints: git-snapshot layer (primary); `/rewind` only if verified headless. **18.** Structured output, web-search rendering, image/PDF attach, `--worktree`, `--remote-control` as opt-in flags. **19.** Evaluate ACP as the long-term unifying seam — adopt only if multi-agent breadth outweighs Claude-specific fidelity.

---

## 3. The `AgentBackend` adapter seam

Introduce the trait *before* Codex, sliding it under today's call sites. `TurnSpec` and `AgentEvent` are already backend-agnostic in spirit — formalize that.

```rust
// backend.rs
pub trait AgentBackend: Send {
    /// claude accepts a caller-minted uuid; codex generates one and we
    /// must parse it from thread.started.
    fn session_id_policy(&self) -> SessionIdPolicy; // CallerMinted | ParseOnFirstEvent

    /// Persistent-child (P1) or per-turn (P0). Emits normalized AgentEvents.
    fn spawn_turn(&self, spec: &TurnSpec, tx: UnboundedSender<Msg>) -> BackendHandle;

    /// aeovim env / $AEOVIM_PIPE contract injection.
    fn inject_system_context(&self, env: &EnvContext, cmd: &mut Command);

    /// One abstraction over divergent permission surfaces.
    fn apply_permissions(&self, p: &Permission, cmd: &mut Command);

    fn fork(&self, session_id: &str) -> anyhow::Result<String>;
    fn interrupt(&self, handle: &BackendHandle);            // P1
    fn steer(&self, handle: &BackendHandle, msg: &str);     // P1
}

pub enum Permission {
    Plan, AcceptEdits, Default, Yolo,   // union of both backends; sandbox axis codex-only
}
```

`ClaudeBackend` is a near-verbatim move of current `agent.rs`. Divergences a `CodexBackend` must handle:

| Concern | Claude | Codex |
|---|---|---|
| Core spawn | `claude -p … --output-format stream-json` | `codex exec --json "<prompt>"` |
| Session id | caller-minted `--session-id <uuid>`; `--resume` | **generated**; parse `thread.started.thread_id`, then `codex exec resume <id>` |
| Event granularity | token deltas + `assistant` + `result` | item-level `item.started/completed` (`agent_message`, `command_execution`, `file_changes`, `mcp_tool_call`, `plan_update`), `turn.completed` |
| System context | `--append-system-prompt` (agent.rs:53–54; `env_prompt_for` at app.rs:1317) | **no equivalent** → prepend env block to prompt on turn 1 and/or write `AGENTS.md` in cwd |
| Permissions | one enum `--permission-mode` / `--dangerously-skip-permissions` | two axes: `--sandbox {read-only\|workspace-write\|danger-full-access}` + `--ask-for-approval {untrusted\|on-request\|never}`; YOLO = `--yolo` |
| cwd / git | any dir | must pass `--skip-git-repo-check`; default sandbox `read-only` (forget it → silent no-writes) |
| Config lever | separate flags | any key via repeated `-c key=value` |
| Interactive approval | control protocol (P1) | impossible in `codex exec`; needs `codex app-server` JSON-RPC |

Parsing stays per-backend (a `codex_protocol.rs` mapping items → shared `AgentEvent`) so schema drift can't leak into the enum. **Desktop / other agents** plug in via MCP, not a new backend: honor the same `.mcp.json`/`~/.claude.json` so Desktop-configured servers work, and expose `claude mcp serve` if an external driver needs aeovim's Claude toolset.

---

## 4. Decisions / tradeoffs (with the corrections folded in)

- **Persistent child vs per-turn.** Per-turn for P0 (matches today, lowest risk, unblocks all rendering immediately). One persistent child per chat for P1 — the *only* way to get canUseTool approval, AskUserQuestion, MCP elicitation, `Esc`-interrupt, and mid-turn steering, and it lets `$AEOVIM_PIPE` inject into an in-flight turn. Cost: lifecycle/error/resume handling in `agent.rs` gets materially harder. Explicit milestone.

- **§A — built-in slash commands are not prompt text.** Custom commands/skills (`.claude/commands`, `.claude/skills`) *are* prompt expansions and work as turn text. Built-in interactive commands (`/context`, `/memory`, `/model`, `/rewind`) are handled by the REPL, not the model; sent to `claude -p` they most likely arrive as literal text and produce a confused answer. So: `:context` is built from `usage.*`, `:memory` edits the file, `/compact` is used only after verifying its headless behavior, and `/rewind` is treated as unverified with git-snapshot-per-turn as the *primary* mechanism.

- **§C — how approval is wired.** Under `--input-format stream-json`, the sanctioned path is the control-protocol `canUseTool` request on the same pipe (what the Agent SDK does). Do not assume `--permission-prompt-tool` (which expects an MCP tool name); verify against the installed binary before building the modal.

- **§D — MCP is not "inherited for free."** Headless `-p` cannot run the `/mcp` OAuth flow, so servers needing sign-in (GitHub, Sentry, Linear, most remotes) must be authed out-of-band via `claude mcp login --no-browser` in a pane. claude.ai connectors (Gmail/Calendar/M365) load only under a claude.ai *subscription* login, not an API key, and Gmail/Calendar/M365 can't complete OAuth from the CLI at all (connect on claude.ai web first). Project `.mcp.json` servers need workspace trust / `enableAllProjectMcpServers` that `-p` skips. Ship an explicit "authenticate MCP server" action, detect the auth method (`claude /status`), and warn when connectors are expected but the login is an API key.

- **§E — subagent streaming is unverified.** Validate against a live headless run before designing the tree/ephemeral-pane UI; a subagent may only surface as a single `Task` tool_use + final tool_result. Don't build a pane for events that never arrive.

- **§F — system-prompt injection is a correctness risk.** `spawn_for` re-renders `env_prompt_for` (dynamic: sibling-space list, cwd) on **every** turn including resumes. Unknown whether `--append-system-prompt` on a `--resume`d session appends once or compounds each turn (unbounded growth) — and dynamic per-turn content busts prompt caching (`--exclude-dynamic-system-prompt-sections` exists for this). Decide static-at-session-creation vs per-turn; this directly shapes the `inject_system_context` trait method, and Codex has no equivalent flag.

- **§G — cost/usage on resume.** Confirm `result.total_cost_usd`/`usage.*` are per-turn vs cumulative on `--resume` before wiring the footer, else costs double- or under-count.

- **How to intercept tool calls — three tiers.** (a) `--include-hook-events` — observe + statically *gate/deny* today, no persistent child; (b) control-channel `canUseTool` — real interactive allow/deny, needs persistent child (P1); (c) `--allowedTools`/`--disallowedTools` — static rules, back the "always-allow-this-tool" action. Use (a) now for the debug pane and P0 safety, (b) in P1, (c) throughout.

- **Agent SDK vs raw CLI.** Do not embed the TS/Python Agent SDK — aeovim is Rust. Treat the SDK as the *spec* for the control protocol and port the thin stdin/stdout handshake into `agent.rs` (the Go community SDK documents the wire format as a porting reference).

- **Default permission posture.** Preserve `--dangerously-skip-permissions` as an explicit per-space **YOLO** toggle, but make `AcceptEdits` the per-chat default once the approval modal exists, so plan/default modes become reachable without losing speed.

- **ACP now vs later.** ACP collapses Claude+Codex+Gemini+"hermes" into one client but costs a full JSON-RPC client and gives less Claude-specific fidelity (canUseTool, thinking, todos) than the native control protocol. Ship native Claude first (P1); revisit ACP in P2 only if breadth outweighs fidelity. Pick one primary seam — don't double-maintain event mappings.

---

## Open questions for you

These are the calls only you can make. Several block concrete design work downstream.

1. **Auth mode: claude.ai subscription or API key?** Load-bearing — it decides at once whether claude.ai connectors and MCP OAuth work at all (subscription-only), whether P1's persistent-child "SDK-style headless" usage draws from the separate weekly token pool flagged for June 2026, and how much of the §D MCP work is even reachable.

2. **Is interactive tool approval a real requirement, or is `--dangerously-skip-permissions` the permanent posture?** The entire P1 milestone (persistent child, canUseTool, AskUserQuestion, elicitation, interrupt, steer, mid-turn pipe injection) is the costliest work here and only pays off with a human in the loop. If dangerous-by-default is permanent, most of P1 is dead weight and the plan collapses to P0 + Codex breadth.

3. **Which built-in slash commands must work headlessly?** `/context`, `/compact`, `/rewind`, `/memory` almost certainly can't be driven by sending prompt text. Confirm you want native aeovim equivalents (built from `usage` data / git snapshots / direct file editing) instead — this reshapes P0-4 and P1-14.

4. **Static or dynamic system-prompt injection — and does `--append-system-prompt` compound on resume?** Decides whether env context is set once at session creation vs re-injected every turn, whether you adopt `--exclude-dynamic-system-prompt-sections` for cache reuse, and directly shapes `inject_system_context` for the Codex adapter (which has no equivalent flag).

5. **Primary adapter seam — native Claude control protocol first, or ACP now?** Pick one to avoid double-maintaining event mappings. ACP buys Codex/Gemini/"hermes" breadth at the cost of Claude fidelity.

6. **What is "hermes"?** Unconfirmed. The likely match is Nous Research's **Hermes Agent** (`hermes -z "<prompt>"`, OpenAI-compatible server, speaks ACP) — not the Hermes 4 models. This blocks the P2 trait shape and any Hermes backend work. Please confirm the exact tool before we build against it; if confirmed, it drops cleanly behind the same `AgentBackend` trait.