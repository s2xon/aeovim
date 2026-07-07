# aeovim — Implementation Plan

## 0. How to read this plan

This is a sequenced build plan for the design in the seven sections above. It is optimized for **one author building a daily driver**, so it favors a working thin slice you can live in over broad-but-inert scaffolding. The spine is a milestone ladder M0–M8; each milestone is independently demoable and each retires a named technical risk. Three rules govern the whole plan:

1. **Walking skeleton before breadth.** Get one keystroke → one `claude` child → one streamed reply on screen before building tabs, diffs, or orchestration. Everything else hangs off that verified spine.
2. **Verify the `claude` CLI empirically, and do it first.** Every stream-json flag, event shape, field-casing, and control-protocol claim in the design is marked "verified / likely / uncertain." M0's spike turns those into recorded fixtures. No feature is built on an unverified wire fact.
3. **The daily-loop interactions are v1, not fast-follows.** Approving a command, interrupting a runaway turn, seeing two conversations at once, and picking a fan-out winner are the reasons to live in aeovim. Where the draft punted these, this plan pulls a concrete v1 answer forward — even if the *fanciest* form of each stays deferred.

---

## 1. Walking skeleton first (the de-risking slice)

**Before M0's polish, build the thinnest possible end-to-end vertical** and throw most of it away. Target: a single `main.rs` that, in under ~400 lines total, does:

```
raw terminal → type a line → spawn `claude -p --input-format stream-json
--output-format stream-json --verbose --include-partial-messages`
→ read NDJSON lines on a tokio task → print assistant text deltas live
→ print total_cost_usd on `result` → Ctrl-C reaps the child group cleanly
```

No ratatui, no Elm loop, no traits — `println!` is fine. The **only** goal is to prove, on the real binary, that:

- the exact flag set launches and streams (not the invented one),
- a `user` NDJSON line on persistent stdin produces a `result` and a follow-up turn keeps context (`num_turns` increments, `session_id` stable),
- partial `content_block_delta` lines actually arrive with `--include-partial-messages`,
- `killpg` on the process group leaves no orphaned Node/MCP processes.

**Deliverable of the skeleton:** a committed `spikes/` binary plus a `fixtures/` directory of **captured real stream-json transcripts** (a plain turn, a turn with an `Edit` tool_use, a turn with a **non-edit `Bash` tool call**, a turn hitting a **permission denial**, a `system/init`, a `rate_limit_event`, a multi-content-block streaming turn, and a `result` with `permission_denials[]`). These fixtures become the golden inputs for every parser unit test in M1+.

**Two wire facts to nail down here, because the draft got them wrong and they are load-bearing (see R1, R9):**

- **Field casing is mixed.** The `init` line mixes camelCase and snake_case keys — the live capture shows `permissionMode` (camelCase) alongside `mcp_servers` / `slash_commands` (snake_case). A blanket `#[serde(rename_all = "snake_case")]` on an internally-tagged enum only renames *variant tags*, not struct fields, so `permission_mode` would silently never bind. **Capture the real `init` line and annotate every field with its exact JSON key** (e.g. `#[serde(rename = "permissionMode")]`), then add a fixture round-trip test. Audit every camelCase-vs-snake_case field this way rather than assuming.
- **Delta correlation key.** Coalescing token deltas requires a *stable per-message* key. `stream_event` wraps raw Anthropic SSE, where deltas are correlated by content-block `index` under a preceding `message_start` id; the outer envelope `uuid` may be per-line. **Capture a multi-block streaming turn and confirm the key.** Almost certainly coalesce by `(message-id from message_start, content_block index)`, not the per-line envelope uuid. Bake the confirmed key into the `TextDelta` payload.

This is the single highest-leverage day of the project: it converts the design's biggest uncertainty (Design §5.2/§5.3 wire format) into recorded fact and gives the reducer tests deterministic inputs forever.

If any flag in Design §5.2 proves wrong here, fix the design's flag table now, before M0.

---

## 2. Milestone ladder

### M0 — Skeleton: runtime, event loop, modal statusline

**Goal:** a launchable `avim` binary with the Elm/tokio spine, alt-screen lifecycle, and modal input working against *no* agents yet. This is the chassis everything bolts onto.

**Tasks:**
- Cargo workspace; `main` builds a tokio multi-thread runtime, installs alt-screen + raw mode behind a **panic hook + `Drop` guard** and a SIGINT/SIGTERM handler (Design §3.2). Terminal must restore on panic — test by `panic!`-ing mid-run.
- **Enable the kitty keyboard protocol** (`PushKeyboardEnhancementFlags`) at startup and record whether the terminal accepted it. This is a prerequisite for a reliable single-keystroke send in M1 (see the `<C-Enter>` fix there). Ghostty — the author's terminal — supports it.
- `App` struct, `Mode` enum, `PendingInput`, `dirty` flag, `effects: Vec<Effect>` (Design §3.4). `update(&mut App, AppEvent) -> ()` pure reducer stub.
- `UiTask` `select!` loop over `crossterm` EventStream, the UI-facing `mpsc<AppEvent>`, a 40 fps tick, and a `CancellationToken` shutdown (Design §3.3). Draw only on tick when `dirty`.
- `KeymapEngine`: the trie + count/operator resolver (Design §2.6) as a pure `KeyEvent → Resolve`. Wire Normal↔Insert↔Command↔Visual transitions and `:q`/`:qa`. **Do not bind bare `q` to a destructive action** — reserve `q` (aeovim has no macros, but a vim native's reflex is macro-record); close is `:q` plus a `<leader>` bind (see M2).
- `Renderer` skeleton: tabline (placeholder), empty main region, **statusline honoring the contract** (`MODE`, running-job count, cost, context indicator — Design §2.7), command row in Command mode.
- `tracing` + `tracing-appender` rolling file at `~/.local/state/avim/log`; **nothing to stdout** (Design §8.7).

**Crates introduced:** `tokio`, `tokio-util`, `futures`, `ratatui`, `crossterm`, `anyhow`, `thiserror`, `tracing`, `tracing-appender`, `tracing-subscriber`, `crokey`.

**Definition of done / demo:** launch `avim` → land in NORMAL with a rendered statusline and command row → type `:` and see the cmdline, `i`/`Esc` flip modes, a `tui-textarea` composer accepts text → `:q`/`:qa` exits and the terminal is fully restored. Force a panic and confirm the terminal comes back clean with a backtrace in the log. Log the kitty-protocol negotiation result.

---

### M1 — One live agent: spawn, compose, send, stream, render

**Goal:** the walking skeleton, done properly through the adapter seam. One `claude` session as a buffer; type a prompt, send it, watch the reply stream into a ratatui transcript.

**Tasks:**
- Define the **adapter seam now, with one impl** (Design §5.1): `AgentBackend` trait, `SessionHandle`, normalized `AgentEvent`/`Control` (control stubbed). Resist inlining claude specifics into the UI.
- `ClaudeCodeBackend::spawn`: resolve claude by **absolute path** (`which` / known version dir), never the shell alias (Design §5.2, §6.9). Build the verified flag set. Self-mint `--session-id` UUID.
- **Default `--allowedTools` allow-set (the v1 permission answer — see R3/R10 and the blocker note below).** Ship a concrete, config-sourced allow-list covering the safe commands the author actually runs (e.g. `Bash(cargo *)`, `Bash(git status/diff/log *)`, `Read`, `Grep`, common MCP read tools) so that under `acceptEdits` the agent can run tests/build/read-only git without every turn auto-denying. This is a **v1 decision, not an open question.**
- **Reader-side backpressure, stated precisely (fixes the draft's self-contradiction — see R2).** The reader owns an **unbounded local staging buffer**, continuously drains stdout, and **coalesces consecutive text/thinking deltas per message in place** (keyed by the confirmed `(message-id, block index)`). It forwards to the UI channel via `try_send`; on `try_send` failure it keeps *merging deltas into the staged item* (memory is bounded by collapse, not by drop) and **always forwards non-partial events** (`init`/`assistant`/`tool_use`/`tool_result`/`result`), blocking only briefly if truly necessary. Drop the word "bounded" for the delta path — it is "bounded after coalescing."
- **Per-session pending-turn queue (fixes the never-wired completion signal — see R4).** `SessionHandle::send_turn` (and `send_turn_acked`) enqueue a completion `oneshot` onto a per-session FIFO owned by the reader. On emitting a normalized `TurnResult`, the reader **pops and fires the head oneshot** (turns are sequential per persistent session, so FIFO correlation is valid). On interrupt/kill, fire the head oneshot with a `Cancelled` result so any awaiting loop/fanout `select!` unblocks. This is the contract the loop scheduler and fanout throttling depend on — document it as part of `SessionHandle` and the reader task.
- Per-child tasks (Design §3.3): **reader** (as above), **writer** (owns `ChildStdin`, one `user` NDJSON line per turn), **supervisor** (`child.wait()`, `killpg` on cancel, own process group).
- `ClaudeEvent` serde enum (internally tagged, `#[serde(other)] Unknown`, raw line retained) → map to `AgentEvent` (Design §5.3). Field keys annotated to their **exact JSON casing** (per §1). **Unit-test the mapper against every M0 fixture with `insta` snapshots.**
- `Agent`/`Message`/`Block` state; reducer appends `TextDelta` to `streaming`, replaces it on `Assistant`, closes the turn and records `total_cost_usd` on `TurnResult` (Design §3.6 walkthrough B).
- Renderer: transcript view (user/assistant/tool-call lines), plain-text only (no highlighting yet). Compose→Send flow (`i`, send, `.` resend).
- **Send binding, terminal-safe (see R11).** Primary send is `<C-Enter>` **only when the kitty protocol negotiated in M0 succeeded**. If not, fall back to a guaranteed single-keystroke Insert-mode send (config default) so submitting a prompt never requires an `Esc`→`<leader><Enter>` mode round-trip. Guarantee one-keystroke send from Insert on the author's real terminal.
- **Interrupt, done right (see R5, and the R7 spike).** `<C-c>` interrupt targets the **control-protocol `interrupt`** (keeps the child alive → next turn is instant and free of a context-reload tax), with `killpg` + `--resume` as the **hard fallback**. The control path is gated on the early control-protocol spike (see M1-adjacent spike below); until it passes, killpg+resume is active and **interrupt is documented as lossy-by-design**: mark the in-flight streaming message `[interrupted]` in aeovim's transcript, and on cold `--resume` trust the child's next `init`/first turn over aeovim's optimistic partial (avoids a diverged dual-written NDJSON).
- Version-gate on `Init.claude_code_version` (Design §5.3): warn + degrade on drift.

**Control-protocol spike (runs alongside M1, low-risk probe — informs R7/M8).** Exercise `initialize` + `control_request{subtype: interrupt}` live and record the real envelope. Interrupt is the highest-frequency steering verb, so proving the live path early is worth the day. **Do not** build the pre-apply manual gate here (that stays M8) — the spike's only v1 payoff is live interrupt; if it fails, killpg+resume remains the truth.

**Crates introduced:** `serde`, `serde_json`, `uuid`, `slotmap`, `tui-textarea`, `dirs`.

**Definition of done / demo:** `avim` opens with one agent tab; press `i`, type "refactor this function", send → the assistant reply streams token-by-token into the transcript; a follow-up turn keeps context; the statusline shows accumulated cost; the agent can run `cargo test` without an auto-deny (allow-set works); `<C-c>` interrupts a running turn (live control if the spike passed, else killpg+resume with an `[interrupted]` marker); closing reaps the child with no orphaned processes (verify with `ps`).

---

### M2 — Many agents: tabs, motions, splits, close

**Goal:** the multiplex layer. N concurrent agents as tabs, vim navigation between them, and a **real side-by-side view of two conversations** (fixing the draft's under-delivery of the "multiple conversations on screen" vision).

**Tasks:**
- **One agent ≈ one tab (fixes the gt/tab muddle — see R12).** For v1, `gt`/`gT`/`{count}gt` cycle **agents**. Splits are *views within the workspace*, not a competing "tab = layout" abstraction. The mental-model table and the keymap must agree: drop the "tab = window tree / layout" framing until arbitrary multi-agent layouts land as a fast-follow (at which point a *distinct* bind handles layout-tabs and `gt` stays agent-cycle).
- `Tab`/`WinNode`/`Window`/`View` window tree (Design §3.4). v1 scope, expanded past the draft: **(a)** the mandated transcript‖diff vsplit of the *focused* agent (diff pane empty until M3), **and (b)** a minimal **two-agent vsplit** (agent-A transcript | agent-B transcript). The `WinNode` tree already supports it; the only new work is picking which agent each leaf shows and routing focus. This delivers "multiple conversations side by side" in v1 rather than gesturing at it. Arbitrary N-way trees stay deferred.
- Motions: `gt`/`gT`/`{count}gt` (agents), `<C-w>h/j/k/l`, `<C-w>v/s/c/o/=`. `ga` fuzzy **agent picker** overlay (Design §2.3).
- **Split the close verb (see R13).** `:q` / `<leader>` bind = **hide/detach** (view closes, agent keeps running, stays on the board as a background job — matches vim's buffer-hide). `:bd!` / `<leader>K` = **kill + reap** the child. Prompt if a Job is live on the kill path. This preserves in-flight work; clearing screen clutter never silently kills.
- `:new [model] [--cwd] [--worktree]`, `:b {id}`, **`:ls`/`:agents` list only aeovim-owned sessions in v1** (see R14). The `claude agents --json` merge is version-fragile and stays deferred to the M8 background-agents fast-follow (or best-effort: parse defensively, drop on shape mismatch). Spawn semaphore capping concurrent **live** children (Design §3.3; default an open question, start at 6).
- **Backpressure proof:** run several streaming agents at once; confirm the unbounded-staging + coalesce + `try_send` path keeps the focused agent's redraw smooth and unfocused agents update as a tabline spinner without stalling (Design §3.3). This is the milestone that retires the "many jobs, one render loop" risk.
- Tabline status glyphs (streaming/needs-input/loop/idle/error) fed from agent status.

**Crates introduced:** `nucleo` or `fuzzy-matcher` (agent/skill picker).

**Definition of done / demo:** spawn 4 agents, send a long prompt to each; `gt`/`gT`/`3gt`/`ga` jump instantly between them while all four stream; a two-agent vsplit shows agent-1 and agent-2 transcripts *simultaneously*; `:q` detaches a still-working agent (it keeps running, visible on the board), `:bd!` kills+reaps one. Streaming in tab 4 never freezes typing in tab 1.

---

### M3 — Diff review: hunks, `]c`/`[c`, approve/reject on the git tree

**Goal:** vim-native code-diff review against a **per-turn baseline** of the git working tree (Design §6), tied to `acceptEdits` permission mode.

**Tasks:**
- Launch editing agents with `--permission-mode acceptEdits`; **each fanout/isolated agent gets its own `git worktree`** (Design §6.3). The **solo agent** starts in the repo's main tree — but see the baseline fix below.
- **Per-turn baseline, not raw HEAD (fixes WIP pollution — see R15).** A daily-driver repo is usually already dirty. Diffing `git diff HEAD` would mix the author's uncommitted WIP with the agent's edits, so `]c`/`s`/`x` could operate on the wrong author's hunks. At **turn start**, snapshot the working-tree state of touched files (e.g. a `git stash create` ref or a content snapshot) and diff the agent's post-turn tree against **that** baseline. The review pane then shows only what *this turn* changed. (Defaulting the solo agent into its own worktree is the alternative; the per-turn snapshot is cheaper for the common single-agent case.)
- `DiffReviewer` (Design §6.10): on each turn `result`, and on a debounced `notify` fs event, compute the diff against the per-turn baseline + `git status --porcelain=v1` as **effects off the UI task**; re-diff with `similar` to own hunk boundaries + inline word diff (Design §6.4/§6.5).
- **Scope the watcher (see R16).** The `notify` watcher must be **gitignore-aware** (skip `.git/`, `target/`, `node_modules/`, and anything git-ignored), **debounced ~200–300 ms**, and re-diff only files git actually reports changed. Prevents constant re-diff churn/flicker during builds and formatter runs.
- `FileDiff`/`Hunk`/`DiffLine` model; hunk cursor is a **`HunkId`, not a line** (survives mid-review refresh — Design §6.7).
- **Approve/reject state is aeovim-owned, not rebuilt from git (fixes the stage-doesn't-hide contradiction — see R6).** The reviewer diffs working-tree vs the per-turn baseline; **approve is pure aeovim-side bookkeeping** (`HunkState::Approved`), re-anchored on `HunkId` across re-diffs, and **does not stage** (staging wouldn't change the diff anyway). Reject actually mutates the tree. Drop the claim that hunk state is "fully rebuilt from git after every op" — the *tree* is truth for hunk *content*, but approve/reject *state* is aeovim-owned and must survive refresh.
- Diff pane renders in the vsplit; `]c`/`[c`/`]C`/`[C`/`{count}]c` navigation; gutter signs; statusline `hunk i/N` + approved/rejected counts (Design §6.6).
- **Reject/approve keys — present as aeovim's own scheme (fixes the over-sold fidelity claim — see R17).** `s` = approve (aeovim bookkeeping), `x` = reject (`git apply -R` single-hunk patch, fallback worktree-only, `git restore` for whole-file adds), `S`/`X` whole-file, `u` LIFO undo stack, `v`+`s`/`x` visual multi-hunk. **Do not** market these as "faithful gitsigns/fugitive muscle memory" — they match neither (fugitive is `s`/`u`; gitsigns is `<leader>hs`/`<leader>hr`). Either adopt one of those conventions or own the scheme honestly; `s`/`x` don't collide in transcript context. Re-diff content after every reject op.
- **Proactively re-sync the agent after a reject (fixes the "expected desync" footgun — see R18).** When a hunk is reverted, the working tree no longer matches what the agent believes it wrote, so its next `Edit` fails on a stale `old_string` and wastes a turn. On the next turn, **inject a short note** listing the files/regions reverted and why, so the agent re-reads instead of failing an exact-match edit. Cheap — aeovim already holds the reverted patch.
- `:perm` cycles mode via **restart + `--resume`** (v1 truth; live control deferred). **Collapse `manual`/`default` to one cycle entry** — research says `manual` is an alias of `default`; confirm the canonical name via `claude --help` on the pinned version and document the alias (see R20). Surface `result.permission_denials[]` with full `tool_input` as a read-only preview (free, no control protocol — Design §6.2 note).

**Crates introduced:** `similar`, `notify`.

**Definition of done / demo:** with pre-existing WIP in the repo, ask an agent to edit 2–3 files; when it finishes, the right pane shows **only the agent's hunks** (not your WIP); `]c`/`[c` walk them, `x` reverse-applies a hunk (verify on disk + `git status`), `u` restores it, `S` keeps a whole file; approve state survives a mid-review fs-triggered re-diff. Reject a hunk the agent depended on and confirm the next turn gets an injected re-read note instead of a blind failed `old_string`.

---

### M4 — Orchestration: job registry, board, fan-out (with harvest), loop scheduler, skills

**Goal:** the orchestrator. Every unit of agent work becomes a `Job` on a quickfix-style board; fan-out **including pick-a-winner harvest** and the aeovim-owned loop scheduler ship; skills/subagents are surfaced (Design §4).

**Tasks:**
- `Job`/`JobKind`/`JobState` slotmap; reducer derives state transitions from normalized events per the Design §4.1 table (no polling; driven by the M1 pending-turn/`TurnResult` signal). `cost_usd` summed per job / per group / global; per-job `budget` self-cancels on breach.
- **Budget authority is aeovim's accounting, not the CLI flag (fixes double-governance — see R21).** Enforce loop/fanout budgets from aeovim's summed `total_cost_usd` (cancel via token). Reserve `--max-budget-usd` as a **coarse per-child hard backstop set well above** aeovim's soft cap. A CLI budget abort surfaces as `result:error` → red job; aeovim's cap should trip first.
- **Task Board** overlay (`<leader>t`/`:tasks`): `ratatui` `Table`, green/yellow/red + spinner/dim, `]j`/`[j`, `<CR>` jump-to-agent, `dd` cancel (`token.cancel()` + `killpg`), `r` restart, subagent nesting via `parent_tool_use_id` (Design §4.7).
- **A v1 permission-approval path on the board (the daily-loop blocker — see R3 blocker + R10).** Non-edit tools (Bash/MCP/git) that fall outside the M1 allow-set still auto-deny headless into `result.permission_denials[]`. Surface each denial as a **yellow board row** with an **`approve+retry` keybind** that re-issues the turn with that specific tool added to `--allowedTools` (persisting to config if the author chooses). Combined with the M1 default allow-set, this gives v1 a real, fast approve loop **without** the control protocol. (The control-protocol `can_use_tool` pre-apply gate remains the M8 upgrade, not the v1 requirement.)
- **Fan-out with harvest (fixes the half-feature — see R19).** `:fanout N <prompt>` / explicit ids: N spawns each in its own worktree, grouped, bounded by the spawn semaphore, permit held until `result` **via the M1 pending-turn oneshot** (Design §4.4). Board group header with done/total + aggregate cost + cost-warning line. **Ship the winner-picking action in M4, not M8:** `gm` = adopt this member (git merge/cherry-pick its branch into the main tree), `gd` = discard (`git worktree remove --force`). Conflict handling may stay crude initially (bail to a message), but the pick-a-winner verb **must exist in v1** or fan-out isn't usable.
- **Loop scheduler** (aeovim-owned tokio task, **not** claude `/loop`): `:loop [ival] <prompt|/cmd>` / `:loop stop`, awaiting each turn via the M1 result-oneshot (Design §4.5). **Self-paced loops get a real floor and a completion protocol (fixes the busy-loop money-burner — see R22):** a configurable minimum inter-iteration delay (default ~30 s) instead of `Duration::ZERO`; a **concrete completion signal** — the loop prompt instructs the agent to emit a defined sentinel (a specific line or `stop_reason`), and the loop stops when the sentinel is seen or the cost cap hits. The **cost cap is non-optional for self-paced loops**, and the board shows next-fire time. Handle the acceptEdits-denies-non-edit-tool stall as the same **yellow board approve+retry** row as above (Design §4.5 open decision, now resolved).
- **Skill/slash palette** (`<leader>s`/`:skills`): populate names from `system/init` (`slash_commands`, `skills`, `agents`, `mcp_servers`); invoke as a slash-prefixed `user` turn — inherited, not reimplemented (Design §4.3). **Scope arg entry honestly (see R23):** `init` enumerates *names*, not argument schemas, so v1 offers name fuzzy-match + free-text arg entry. Structured "Tab-complete args" is only possible by parsing `.claude/skills/*/SKILL.md` / command frontmatter off disk — a fast-follow, not an init-backed claim.
- **Cancellation-token tree** + **watchdog** for silently-dead jobs (Design §4.8).
- Background agents (`claude --bg` + `claude agents --json` polling) explicitly **deferred to M8** — flag/JSON shape is version-sensitive (Design §4.6). Stub the board row only.

**Crates introduced:** none new (uses `tokio::time`, `tokio_util`, `slotmap`).

**Definition of done / demo:** `:fanout 3 "add rate-limit middleware three ways"` spawns three worktree'd agents visible as a group on the board with live status + aggregate cost; `<CR>` jumps into a member's transcript+diff; `gm` adopts one member into the main tree and `gd` discards the rest; `dd` cancels one cleanly. A `:loop 30s /check-ci` runs on a real interval (never busy-loops), shows run-count + spend + next-fire time, stops on its sentinel or budget. A denied `Bash` command shows as a yellow board row and `approve+retry` re-runs it allowed. `<leader>s` lists real skills from init and invokes one.

---

### M5 — tree-sitter highlighting (syntect floor first)

**Goal:** code inside transcripts and diffs is highlighted; grammar fragility can never break a release (Design §7).

**Tasks:**
- **Stage 1 (permanent floor):** `syntect` + `two-face` highlighter for all languages via fenced info-string.
- **One cache mechanism, streaming handled correctly (fixes the per-token leak + `OnceCell` misuse — see R24).** Use a memoized/`OnceCell` field **only for settled messages**, keyed by their *final* content hash, with an **LRU bound** as belt-and-suspenders. Render the in-flight **streaming message on a separate non-cached fast path** (rebuilt each dirty tick, **no hash insert**) — a content-hash `HashMap` would otherwise gain one entry per token and never evict. Insert into the settled cache only once the authoritative `assistant`/`result` event finalizes the message. Reconcile Design §3.4 and §7.5 to describe this single cache.
- **Stage 2 (pinned grammars):** `tree-sitter` + `tree-sitter-highlight`; `HighlightConfiguration` per pinned language built once at startup, **ABI-range guard → silent per-language syntect fallback** (Design §7.3/§7.7). Pin exact grammar crate versions (Rust, TS/JS, Python, Bash, JSON, Markdown).
- **Write each `build_*()` against its crate's actual surface (fixes the hardcoded-constant compile error — see R25).** Not all grammars export `INJECTIONS_QUERY` (e.g. `tree-sitter-json` has none); some export `HIGHLIGHT_QUERY` (singular) vs `HIGHLIGHTS_QUERY`. Pass `""` for absent injection/locals queries and match the exact constant name per crate. **Verify the ABI-version accessor name** on tree-sitter 0.26 (`Language::abi_version()` vs older `version()`) and guard accordingly.
- Map captures → `HL_NAMES` → TOML-driven ratatui `Style` table (Design §7.4). Apply to transcript code blocks and M3 diff bodies.
- Handle the **unterminated-fence streaming case** (provisional highlight, re-highlight once on close — Design §7.5).
- Structural motions (`]f`/`[f`/`%`/`]]`) explicitly **fast-follow** — parse trees are a free byproduct but out of v1 must-haves (Design §7.6).

**Crates introduced:** `syntect`, `two-face`, `tree-sitter`, `tree-sitter-highlight`, and pinned `tree-sitter-{rust,typescript,python,bash,json,md}`.

**Definition of done / demo:** a streaming ` ```rust ` block colorizes live without frame stalls and **without unbounded cache growth** (watch memory over a long reply); each pinned grammar loads (or logs a clean fallback); deliberately break one grammar's ABI and confirm that language silently drops to syntect while the app runs. Diff hunks are colorized, not monochrome.

---

### M6 — Config, keymap, and persistence

**Goal:** avim becomes livable and restartable — but scoped to a **solo, single-user, local** driver, not enterprise infra (fixes the over-built §8 — see R26).

**Tasks:**
- **Trimmed config for v1:** **one** `config.toml` + **one** `keymap.toml`, typed via serde+`toml`; unknown keys warn, don't abort; `:reload` + `notify` hot-reload (Design §8.5). **Drop the 4-layer precedence and project-local overlay** until there's a real need. (The default `--allowedTools` allow-set and backend registry live here.)
- `keymap.toml` in crokey notation → `Action` variants compiled into the trie; unknown action = statusline error, previous map stays live; `"noop"` unbinds (Design §8.6).
- XDG-first paths (neovim-style even on macOS), `dirs` fallbacks (Design §8.2).
- Workspace resolution by git-root/cwd hash (Design §8.3); per-workspace `state.json` (tabs/windows/agents/focus) + `jobs.json` (loops/fanout/budgets), **atomic debounced writes** (temp + rename) — Design §8.4. **Keep the genuinely valuable bits** (workspace restore, self-minted session-id cache, atomic writes); **defer** `schema_version` migration tooling until the schema first bumps, and **defer** the single-instance lock file and retention sweep.
- Dual-write normalized transcript to `sessions/<uuid>.ndjson`; **lazy detached-first restore** — replay transcript instantly, spawn `--resume` only on first interaction; `warm_focused` option; **loops restore paused** (Design §8.4).
- `:messages`/`:redir` debug views (Design §8.7/§8.8).

**Crates introduced:** `toml`, `blake3` (workspace hash), `serde` (already present).

**Definition of done / demo:** remap `gt`→something in `keymap.toml`, `:reload`, it takes effect without restart. Quit avim with 4 agents + a loop; relaunch in the same repo → tabs/transcripts restore instantly as **detached**, the loop comes back **paused/yellow**, and focusing an agent cold-resumes it. Launch elsewhere → a different isolated workspace.

---

### M7 — Model switching + adapter-seam hardening

**Goal:** `:model`/`:effort` switching proven, and the seam validated so a second backend is a drop-in (Design §5.6/§5.7).

**Tasks:**
- `:model {alias|id}` / `:effort {…}` via **restart + `--resume`** (verified path) with focus/scroll preserved. **Reduce the switch tax (see R27):** (a) **warn in the UI** that a switch reloads context and costs a cache reload; (b) if no turn is in flight, **apply the new model to the *next* turn's flags without an immediate respawn**; (c) keep live `set_permission_mode`/model control behind the stubbed `send_control`, to be replaced if the M8 spike proves it. Don't make restart+resume a silent tax on a frequent action.
- **Seam audit:** grep the UI/keymap/orchestrator for any `claude`/stream-json leak; ensure all claude-only fields (`parent_tool_use_id`, cache usage, `permission_suggestions`) ride as `Option` on normalized events.
- **Proof of seam:** implement a `MockBackend` (or a minimal second real CLI) behind `AgentBackend` that emits normalized events from a fixture; confirm the entire UI, board, and diff flow work against it with **zero UI-layer changes**. This retires the adapter risk with evidence, not assertion.
- Backend registry entries in config; `:model` stays within a backend, backend choice at spawn.

**Crates introduced:** none.

**Definition of done / demo:** `:model opus` on an idle agent applies to the next turn with no respawn; on a mid-turn agent it restarts+resumes with a UI cost warning and keeps the transcript. A `MockBackend` agent tab streams, appears on the board, and shows a diff using only normalized events — demonstrating the Codex/direct-API path is a new impl, not a refactor.

---

### M8 — Control-protocol fast-follows (gated, post-v1)

**Goal:** the features Design flags as *uncertain* — only after a POC proves the wire format. (The M1 spike already probed `initialize`+`interrupt`; this milestone builds on that.)

**Tasks (each gated on its own spike):**
- **Manual pre-apply gate, correctly framed (fixes the mode/canUseTool conflation — see R7).** It depends on **two** things, stated explicitly: (a) a permission mode that does **not** pre-approve edits (verify whether that is `default` or `manual` against a live probe — prior art uses `default`), **and** (b) the `initialize` handshake **registering `can_use_tool`** (a mode name alone does not enable per-call gating; `acceptEdits`/allow-rules bypass `can_use_tool` for edits entirely). Only once both are confirmed, build the pre-apply gate (`:perm manual`, whole-call preview using the same hunk widget — Design §6.9). Keep post-apply git review as the primary surface regardless.
- Live `set_permission_mode`/model swap (replace restart+resume if proven).
- Real interrupt via control message as the *default* (promote from the M1 fallback if the spike hardened it), keeping `killpg` as the hard fallback.
- Background agents: `claude --bg` + `claude agents --json` polling into the board, plus the deferred `:ls`/`:agents` merge (Design §4.6) — parsed defensively, dropped on shape mismatch.
- Structural tree-sitter motions `]f`/`[f`/`%`/folds (Design §7.6).
- Arbitrary window splits beyond the v1 vsplits.
- Fanout worktree **conflict resolution** beyond the M4 crude-bail (real merge UX for overlapping edits — Design §4.4/§8.3).
- Restore the deferred §8 infra only if it earns its keep: 4-layer/project-local config, `schema_version` migration, single-instance lock, retention sweep.

**Definition of done / demo:** each ships independently once its spike passes; none is allowed to regress the verified v1 paths.

---

## 3. Top technical risks → which milestone retires each

| # | Risk | Why it's dangerous | Retired by | How |
|---|------|--------------------|-----------|-----|
| R1 | **`claude` stream-json flags/events are wrong or drift** | The whole app is built on this wire format; §5 marks several claims "likely/uncertain" | **Skeleton + M1** | Live spike captures real fixtures; parser unit-tested against them; `Unknown` fallback + version-gate on `init` make drift non-fatal |
| R9 | **Init/other lines mix camelCase and snake_case keys** | `permissionMode` never binds under blanket `rename_all`, silently breaking perm/version logic | **Skeleton + M1** | Annotate every field with its exact JSON key; fixture round-trip test; audit all casing |
| R2 | **"Bounded channel + always drain + never drop non-partial" is self-contradictory** | A full bounded mpsc blocks the reader → stops draining stdout → blocks the child | **M1 + M2** | Reader owns unbounded staging, coalesces deltas in place, `try_send` (merge-on-fail), always forwards non-partial; proven with 4+ streamers |
| R4 | **Turn→result completion signal never wired across reader/reducer** | Loops never advance and fanout permits never release → spawn-semaphore deadlock | **M1** | Per-session FIFO of completion oneshots owned by the reader; fired on `TurnResult`; `Cancelled` on interrupt |
| R3 | **Non-edit tools auto-deny under `acceptEdits` (the daily loop)** | Approving Bash/MCP/git is the most frequent interaction and has no headless path | **M1 + M4** | Config default `--allowedTools` allow-set + yellow-board `approve+retry`; control gate stays M8 |
| R5 | **Interrupt via kill+cold-resume is slow and re-taxes context** | Steering is constant; every interrupt pays respawn latency + cache reload | **M1 (+M8)** | Early control-protocol `interrupt` spike; live interrupt keeps child alive; killpg is fallback; lossy-by-design documented |
| R6 | **"Approve = git stage" doesn't remove a hunk from the diff, and re-diff clobbers state** | Approve does nothing visible or gets wiped each refresh | **M3** | Approve is avim-owned `HunkState`, re-anchored on `HunkId`, not staged; drop "rebuilt from git" claim |
| R15 | **Solo-agent diff baseline = HEAD mixes your WIP with the agent's edits** | You could reject your own hunk in a dirty repo | **M3** | Snapshot a per-turn baseline of touched files at turn start; diff against that |
| R2b | **Async render loop stalls under many streaming jobs** | N children flooding `content_block_delta` can starve the focused redraw | **M2** | Coalesce-on-fail + draw-on-tick-when-dirty; proven with 4+ concurrent streamers |
| R24 | **Span cache leaks one entry per token; `OnceCell` can't rebuild the streaming msg** | Long replies grow the cache unbounded; contradictory cache mechanisms | **M5** | Settled-only LRU cache keyed by final hash; streaming rendered on a non-cached fast path |
| R25 | **tree-sitter grammar ABI / query-constant / C-toolchain skew breaks the build** | Grammar/core ABI skew + `INJECTIONS_QUERY` hardcode is the single most likely breakage | **M5** | syntect floor always present; per-grammar `build_*()` against real surface; ABI guard → silent fallback; exact pins |
| R7 | **Manual gate conflates permission-mode with `can_use_tool` routing; protocol unverified** | Betting v1's headline on an inferred handshake is reckless | **M8 (spike first)** | Gate needs both a non-pre-approving mode *and* `initialize` registering `can_use_tool`; verified before build |
| R19 | **Fan-out ships without harvest** | "Try N, pick the winner" with no adopt/discard is half a feature | **M4** | `gm` adopt / `gd` discard shipped alongside fan-out; conflict UX crude but present |
| R22 | **Self-paced loop busy-loops and has no completion signal** | `Duration::ZERO` re-fires instantly; money burns between glances | **M4** | Configurable delay floor + sentinel completion + non-optional cost cap + next-fire on board |
| R12 | **gt/tab model is internally contradictory** | Ambiguity confuses implementation and violates vim muscle memory | **M2** | Collapse to agent ≈ tab for v1; drop "tab = layout" until real layouts land |
| R14 | **`claude agents --json` (version-fragile) pulled into early M2** | Unverified JSON contract blocks an early milestone | **M2 (defer merge to M8)** | `:ls` lists avim-owned only; agents-json merge best-effort/deferred |
| R21 | **`--max-budget-usd` and avim's per-job budget double-govern** | CLI may abort the child mid-loop independent of avim accounting | **M4** | avim summation is the authority; CLI flag is a coarse backstop above the soft cap |
| R6b | **Loop scheduling / cancellation leaks or runs away** | Unattended loops spend money; orphaned Node/MCP procs | **M4** | avim-owned scheduler + cancellation-token tree + `killpg` + budget caps + watchdog; restore-paused in M6 |
| R11 | **`<C-Enter>` send undeliverable without kitty protocol** | On plain terminals the only send is an `Esc`→`<leader><Enter>` round-trip | **M0 + M1** | Negotiate kitty protocol at startup; guaranteed single-keystroke Insert send fallback |
| R13 | **Close conflates kill and hide** | Clearing clutter kills in-flight work | **M2** | `:q` = detach/hide (keeps running), `:bd!`/`<leader>K` = kill+reap |
| R26 | **§8 persistence/config is enterprise-grade for a solo driver** | 4-layer config, migration tooling, lock files fight the fast-solo goal | **M6** | Trim to one config + one keymap; defer lock/retention/migration/overlay |
| R6adapter | **Adapter seam leaks claude specifics, blocking a 2nd backend** | Goal-6 promise becomes a lie if stream-json bleeds into the UI | **M1 (built) + M7 (proven)** | Trait at first impl; MockBackend drives full UI with zero changes as evidence |
| R8 | **Terminal left corrupted on panic/crash** | A TUI that bricks the terminal is unusable as a daily driver | **M0** | Panic hook + `Drop` guard restore alt-screen before logging; tested by deliberate panic |

---

## 4. Starter `Cargo.toml` dependencies

```toml
[dependencies]
# runtime / async
tokio        = { version = "1.52", features = ["full"] }
tokio-util   = { version = "0.7", features = ["rt"] }   # CancellationToken
futures      = "0.3"
async-trait  = "0.1"

# TUI
ratatui      = "0.30.2"
crossterm    = { version = "0.29.0", features = ["event-stream"] }
tui-textarea = "0.7"                                     # confirm 0.30 compat (open Q)

# input
crokey       = "1.4"

# serde / data
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
toml         = "1.1"
uuid         = { version = "1", features = ["v4"] }
slotmap      = "1"

# diff
similar      = { version = "3.1", features = ["inline"] }

# fs / paths
notify       = "8.2"
dirs         = "6"
blake3       = "1"

# fuzzy pickers
nucleo       = "0.5"                                     # or fuzzy-matcher

# highlighting — syntect floor first
syntect      = "5.3"
two-face     = "0.5"
# tree-sitter (M5) — pin exact grammar versions; a cargo update can swap export shape
tree-sitter            = "0.26.10"
tree-sitter-highlight  = "0.26.10"
tree-sitter-rust       = "0.24.2"
tree-sitter-typescript = "0.23.2"
tree-sitter-python     = "0.25.0"
tree-sitter-bash       = "0.25.1"
tree-sitter-json       = "0.24.8"
tree-sitter-md         = "0.5.3"

# logging / errors
tracing            = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender   = "0.2"
anyhow             = "1"
thiserror          = "2"

[dev-dependencies]
insta        = "1"    # snapshot-test the stream-json → AgentEvent mapper against fixtures
```

Introduce tree-sitter crates only at M5 (behind the syntect floor) so an ABI problem can't block earlier milestones. Pin exact versions on the grammar crates — a `cargo update` can silently swap their export shape, and different crates export different query-constant names (Design §7.7; write each `build_*()` against its crate's real surface, per M5).

---

## 5. Open questions to resolve (before / along the way)

**Resolve during the walking skeleton (block M1):**
- Confirm the exact verified flag set on the installed `claude` version, incl. `--replay-user-messages` behavior and whether `--verbose` is still mandatory with `-p`+stream-json.
- Confirm multi-turn on persistent stdin keeps context and both `user` content shapes (string vs `[{type:text}]`) are accepted.
- Confirm `--include-partial-messages` actually emits `content_block_delta`, and capture a real `result.permission_denials[]` payload.
- **Confirm the exact JSON casing of every `init` (and other) field** (`permissionMode` camelCase vs `mcp_servers`/`slash_commands` snake) — annotate structs and add a round-trip fixture test.
- **Confirm the delta correlation key** on a multi-block streaming turn — coalesce by `(message-id from message_start, content_block index)`, not the per-line envelope uuid.
- Confirm the canonical permission-mode name (`manual` vs `default` alias, presence of `auto`) via `claude --help` on the pinned version.

**Resolve during M2–M3:**
- **Spawn-semaphore default cap** and **per-agent `max_budget_usd` default** (Design §3.3/§4.8; blueprint suggests ~4–6 / $5).
- Confirm `tui-textarea` works with ratatui 0.30, else fall back to `edtui` or a hand-rolled composer (Design open decision).
- **Worktree isolation mechanism:** is `claude -w/--worktree` reliable, or does avim always run `git worktree add` itself and set cwd/`--add-dir`? (Design §6.3.)
- **Default `--allowedTools` contents:** which Bash/MCP/read tools are safe-by-default vs approve-on-demand (M1 allow-set + M4 yellow-row).
- Per-turn baseline mechanism for the solo agent: `git stash create` ref vs touched-file content snapshot (Design §6.3/§6.4).
- Reverse-apply robustness on **overlapping multi-hunk single-file edits** (Design §6.8).

**Resolve during M4–M6:**
- **Idle-session demotion** threshold and cold-`--resume` reconnect latency; is `warm_focused` enough? (Design §3.2/§8.4.)
- **Fanout conflict handling** beyond `gm`/`gd` when members touch overlapping regions (Design §4.4/§8.3).
- Self-paced loop **completion sentinel** design and default inter-iteration floor (Design §4.5).
- When to integrate `claude agents --json` + `--bg` for detached/background rows, given version-sensitivity (Design §4.6).

**Resolve before M8 (control protocol):**
- Exact `control_request`/`control_response` envelope and whether an `initialize` handshake is required to register `can_use_tool` routing to avim (Design §5.6) — the M1 interrupt spike already probes part of this; extend it before committing the manual gate.
- Which permission mode actually leaves edits un-pre-approved so `can_use_tool` fires per Edit (`default` vs `manual`).
- Whether live `set_permission_mode` / model swap works on a persistent stream-json child, or restart+resume remains the truth.
- Per-grammar ABI load verification (and correct ABI-version accessor name) under tree-sitter 0.26 on the actual build host; budget one grammar needing a fork (Design §7.7).