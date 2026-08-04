# External Agents

## What

An **external agent** is a subagent backend whose work is delegated to a
subprocess (or other out-of-process driver) instead of running on an
in-process baybo `AgentActor`. Three are registered today:

- **`claude`** drives `claude` (Anthropic Claude Code) — billed
  against the operator's Claude Code Max/Pro subscription.
- **`codex`** drives `codex` (OpenAI Codex CLI) — billed against the
  operator's ChatGPT subscription.
- **`gemini`** drives `gemini` (Google Gemini CLI) — billed against
  the operator's Google account / Gemini API key.

All three let an baybo subagent's task be handled by the external
agent's own autonomous loop without spending baybo's own per-token API
credit.

Where the LLM crate handles "send messages, get back text" for HTTP
providers, external agents handle "delegate a task, get back a final
result" for binary-backed agents. They're a different shape, not a
different flavor of LLM:

- **Request-response, not conversational.** Each `run()` invocation
  drives the agent through one task and emits a stream of events
  terminating in `FinalContent`. There's no mailbox you keep poking;
  follow-ups happen via a fresh spawn with `resume_session_id`.
- **Autonomous internal tool loop.** claude makes its own
  `Bash`/`Read`/`Write`/`WebFetch` calls. baybo's `sandbox` /
  `sensitive_paths` / `approval gate` do **not** apply to those tool
  calls — see the security note below.
- **External-side state.** Resume continuity lives in the agent's own
  session store (e.g. claude's local session uuid), exposed via a
  `resume_key` event that baybo persists onto the child Session.

## Where things live

- `baybo_model::SubagentBackend` — `Baybo | External { external_kind:
  ExternalAgentKind }`. Carried on `SubagentSpawnRequest.backend`; the
  Baybo backend's model choice travels separately as
  `SubagentSpawnRequest.model_tier`.
- `baybo_model::ExternalAgentKind` — discriminator enum: `Claude`,
  `Codex`, `Gemini`. Carried on `SubagentBackendTag::External {
  external_kind, workspace_dir, resume_key }`.
- `baybo_model::SubagentBackendKind` — discriminator-only view of
  `SubagentBackendTag` for runtime decisions that don't care about
  per-instance state (resume validation, error labels, dispatch).
- `baybo_agent::external_agent::ExternalAgent` — async trait with
  `kind()` (registry key) and `run(request) ->
  Result<Stream<ExternalAgentEvent>>`.
- `baybo_agent::external_agent::ExternalAgentRegistry` — built at boot
  in `crates/baybo/src/runtime.rs`; the spawn router looks impls up by kind.
- `baybo_agent::external_agent::claude_cli::ClaudeCliAgent` — concrete
  impl for `claude -p` (subprocess + stream-json NDJSON parser +
  ETXTBSY-retrying existence check).
- `baybo_agent::runtime::subagent_spawner::ActorSubagentSpawner`
  (`crates/agent/src/runtime/subagent_spawner.rs`) — branches on
  `request.backend`: `Baybo` takes the existing `AgentActor` spawn
  path; `External` looks up the agent, builds a fresh child Session
  (or loads an existing one on resume), runs the agent, persists any
  emitted resume_key, returns a `SubagentResult`.

## Spawn protocol

```jsonc
spawn_subagent({
  "subagent_type": "general-purpose",
  "description": "3-5 word summary",  // trace label; the child never sees it
  "prompt": "self-contained brief",   // the child's first user message

  // Backend selection:
  "backend": "claude",       // or "codex" | "gemini" | "baybo" (default)
  "model_tier": "lite",      // baybo backend only; lite|balanced|deep

  // Optional:
  "background": false,
  "on_timeout": "background",      // or "kill"
  "resume_session_id": "child-...", // continue a prior subagent
  "group": "..."                   // barrier cohort
})
```

There is no per-spawn timeout: baybo children stop at
`max_iterations`, external children at a hardcoded 8-hour idle timeout
(`EXTERNAL_SUBAGENT_TIMEOUT`).

The tool's `to_tool_result_text()` appends
`[subagent_session_id: <id>]` on a successful return so the parent
LLM can echo it back as `resume_session_id` on a follow-up spawn.

## Resume / continue

External agents emit a `ResumeKey` event on the first turn's init
event. The spawn router updates the existing
`SubagentBackendTag::External` tag in place — preserving
`external_kind` and `workspace_dir` — and writes
`resume_key: Some(...)`. Effectively **write-once**: the parser
emits `ResumeKey` only when `is_fresh_session = true` (i.e. the
request carried no prior `resume_key`), and only once per stream.
Subsequent resume calls reuse the stored key without overwriting.

On a resume spawn, the router:

1. Loads the existing child Session and verifies (a) it's not
   `hidden`, (b) its `lineage.parent_session_id` matches the current
   parent, (c) `lineage.kind == Subagent`, (d) the stored backend
   identity matches the requested backend (`stored.kind() ==
   requested_kind`, where `kind()` discards per-instance state like
   `resume_key` and `workspace_dir`).
2. Reads the stored `resume_key` from `SubagentBackendTag::External.resume_key`
   and passes it to the agent's `run(request)`. claude / codex
   forward it as `--resume <uuid>`.
3. Reads the durable `workspace_dir` from
   `SubagentBackendTag::External.workspace_dir` — resume always lands
   in the same on-disk dir picked at genesis.

Continuity survives baybo restarts because the child Session row is
durable.

The same `resume_session_id` field also works for the Baybo backend:
the spawn router loads the existing child Session, the freshly-spawned
one-shot `AgentActor` hydrates the prior transcript via
`restore_transcript_from_store()`, and the new task lands on top of
the restored context.

**Backend identity is durable.** At child-session creation the spawn
router stamps `Session.state.subagent_backend` with the backend's
identity tag — for External, this includes the resolved
`workspace_dir` and `resume_key: None`. Resume validation reads that
field and demands matching identity. Three rejection paths:

1. Backend mismatch — Baybo cannot be resumed as External (or vice
   versa); kind mismatch within External rejected too. Compared by
   projecting both sides to `SubagentBackendKind`, which ignores
   `resume_key` and `workspace_dir`.
2. Untagged session — pre-tag rows or rows created outside the
   spawn router are refused. Operators who need to recover them
   must spawn fresh.
3. External resume with `resume_key: None` — the prior call failed
   before emitting its session-id event. Resuming would silently
   start a new conversation under the existing child_session_id,
   so we refuse and tell the parent to spawn fresh.

## Transcript persistence

Each `run()` call produces an `ExternalAgentEvent::Intermediate(ChatMessage)`
stream alongside the existing `TextDelta` / `ResumeKey` / `Usage` /
`FinalContent` events. The spawn router persists every `Intermediate`
to `session_messages` via `SessionManager::append_session_message`, so
the child session's transcript looks like a normal agent loop:

- Initial `User` message with the spawn request's `task` text
  (recorded *before* `agent.run()` so failures still leave a trace).
- claude: one `Assistant` message per `type:"assistant"` event,
  mapped from claude's content blocks — `text` → `Text`, `thinking` →
  `Thinking`, `tool_use` → `ToolUse` (id and input preserved). One
  `Tool` message per `type:"user"` event carrying `tool_result`
  blocks, with `ToolResult.tool_use_id` linking back to the matching
  `ToolUse`.
- codex: `agent_message` → `Assistant Text`; `reasoning` →
  `Assistant Thinking::Summary`; `command_execution` and `file_change`
  emit a paired `Assistant ToolUse` + `Tool ToolResult`. Tool names
  are codex-prefixed (`codex_shell`, `codex_file_change`) so a reader
  cannot mistake them for baybo-routed tool invocations — these
  bypass baybo's sandbox + approval gate by design.
- gemini: assistant `message` deltas → accumulated into one
  `Assistant Text` per run; `tool_use` / `tool_result` events emit a
  paired `Assistant ToolUse` + `Tool ToolResult` linked by `tool_id`,
  with tool names `gemini_`-prefixed for the same reason as codex's.

`FinalContent` duplicates the last assistant text, so the consumer
treats it as a result signal only and does not double-write.

## Turn lifecycle + trace visibility

`run_external_agent_turn` wraps each run in a `Spawned` turn
(`TurnLifecycle::start_turn` → `start` → terminal transition), the same
turn kind the in-process Baybo backend mints per turn. Without a turn the
child session is invisible to the trace browser:
`QueryApi::list_session_summaries` drops zero-turn sessions and
`GET /v1/traces/{id}` 404s on them. The terminal `SubagentExitStatus`
maps onto the turn: `Completed` → `complete(TurnOutput::Message)`,
`Failed` → `fail`, `Timeout` → `cancel(SubagentTimeout)`, `Cancelled`
→ `cancel(ParentCancelled)`. The run's cancel token is registered via
`register_running`, so an operator-issued turn cancel trips the
subprocess.

External runs record **no step/span tree** — the agent's internal
loop is opaque, and faking `LlmCall` spans would pollute cost /
analytics with calls baybo never made. The persisted
`session_messages` transcript **is** the trace, so the trace viewer
renders it in place of a step tree.

**The wire says so explicitly.** `GET /v1/traces/{session_id}` carries
`external_agent` (`"claude" | "codex" | "gemini"`, absent otherwise)
and `subagent_type`, projected from the durable
`SessionState.subagent_backend` tag by
`QueryApi::subagent_backend_of`. Only the discriminator and the
profile name reach the body — `workspace_dir` and `resume_key` never
do.

That marker exists because *"zero steps"* alone is ambiguous: an
in-process turn that has not flushed its first step yet also has zero
steps. Inferring "external" from a terminal-and-stepless turn — what
the viewer used to do — meant a **running** external agent rendered
nothing at all until it exited, which for an 8-hour idle timeout is
most of the run. With the marker, `isExternalAgentTurn`
(`app/web/src/components/trace/traceTreeModel.ts`) treats the turn as
external the moment its session is known to be, live or not. Sessions
written before the marker reached the wire keep the old
terminal-and-stepless heuristic as a fallback.

Net frontend behaviour (`app/web/src/pages/TraceSessionPage.tsx`):

- The external subagent appears in the Traces list with a `subagent`
  badge + turn status.
- Opening it renders the transcript in the **middle tree pane**, one
  row per message, with each `tool_use` folded together with the
  `tool_result` that answers it — so it reads like the `ToolCall`
  spans of a normal trace rather than like a chat log. Selecting a row
  puts its full text / params / result in the detail panel.
  `buildTranscriptNodes` (`components/trace/transcriptModel.ts`) owns
  that projection.
- Rows appear **as the agent writes them**: the spawn router awaits
  `append_session_message` per `Intermediate` event, and the viewer's
  incremental overview poll picks each one up. A tool call whose
  result has not arrived shows as still running.
- Viewed from the **parent** session, the child's transcript nests
  inline under the `spawn_subagent` span that started it — see
  "Trace viewer polling" in [`webui.md`](webui.md) for the `/lineage`
  endpoint that makes that possible.

## Cost / token accounting

External agents are subscription-billed (claude code Max, codex on
ChatGPT), so their token usage costs the operator nothing per-call.
The `ExternalAgentEvent::Usage` the parser emits is captured by
`run_external_agent` and logged via
`CostManager::record_external_tokens` after the run closes:

- `cost_usd` is **always `MicroUsd::ZERO`** — the method never prices
  the tokens and never touches the daily/monthly budget accumulators,
  so an external run can't trip a spend cap.
- Tokens (input / output / cached / cache-creation) are persisted to
  `cost_records` so the analytics per-session / per-model breakdowns
  include external runs.
- `model` is recorded as `"<kind> (external agent)"` (e.g.
  `"claude (external agent)"`) — deliberately not a priced model slug.
- `span_id` is `SpanId::default()` (nil) since external runs record no
  span tree.

`claude -p` runs with `--permission-mode bypassPermissions`
hardcoded. claude's interactive permission prompts can't reach
baybo's non-TTY subprocess; bypass means every file edit, bash
command, and network call claude decides to make happens with no
further confirmation. **baybo's `sandbox` / `sensitive_paths` /
`approval gate` do NOT apply to claude's internal tool calls.** The
per-spawn `--add-dir <workspace_dir>` only constrains claude's
*default* working area, not its absolute-path reach. Treat
`spawn_subagent(backend: "claude", ...)` as equivalent to
running `claude -p '...'` in a shell on the baybo host.

## claude — where it runs

`<workspace_root>/work/claude/<child_session_id>/` — the dir name is
always the child session id. It is resolved exactly once — at genesis
— and persisted on `SubagentBackendTag::External.workspace_dir`.
Resume calls read it back from the tag; they do not re-derive from the
resume-time request. The dir is created on demand and never
auto-cleaned (matches CLAUDE.md's "session data is core data — never
deleted" rule); operators who want to reclaim disk delete the dir
manually.

## claude — config

```jsonc
"external_agents": {
  "claude": {
    "enabled": false,        // operator must opt in explicitly
    "binary_path": null      // null falls back to PATH lookup
  }
}
```

External agents are **disabled by default** even when their binary
is installed on `PATH`. A claude/codex binary alone is not the
trust signal — the operator must set `enabled: true` (via
`baybo external-agent setup`, `baybo setup`, or by editing
`baybo.json`) to make the LLM able to invoke that backend.

At boot, when `enabled: true`, `ClaudeCliAgent::probe_and_build`
resolves the binary, runs `claude --version` (with an ETXTBSY retry
for editor-just-wrote races), and registers the agent on success.
Probe failure on an enabled kind logs `warn!` but does NOT block
boot — any `spawn_subagent(backend: "claude")` call gets a clear
"not registered" error until the binary is in place. When
`enabled: false`, boot doesn't probe at all and `binary_path` is
ignored.

## codex — invocation

Same shape as claude but a different wire protocol. Built on
`codex exec --json` (the OpenAI codex CLI's non-interactive
JSONL-output mode).

```
codex exec --json --skip-git-repo-check
           --dangerously-bypass-approvals-and-sandbox
           --cd <workspace_dir>
           [--model <name>]
           [resume <thread_id>]
           -- "<prompt>"
```

Event mapping (vs claude's `stream-json`):
- `thread.started { thread_id }` → emit `ResumeKey(thread_id)` (codex
  calls its session uuid a `thread_id`).
- `item.completed { item: { type: "agent_message", text } }` →
  emit `Intermediate(Assistant text)` and accumulate as final
  assistant text.
- `item.completed { item: { type: "reasoning", summary } }` →
  emit `Intermediate(Assistant Thinking::Summary)`.
- `item.completed { item: { type: "command_execution", … } }` →
  emit `Intermediate(Assistant ToolUse name="codex_shell")` followed by
  `Intermediate(Tool ToolResult)`. Name is codex-prefixed so transcript
  readers don't confuse it with an baybo-audited tool call.
- `item.completed { item: { type: "file_change", changes } }` →
  same `ToolUse` + `ToolResult` pair with `name="codex_file_change"`.
- `turn.completed { usage }` → emit `Usage` + `FinalContent`.
- `turn.failed` / top-level `error` → terminal error.
- `item.started` / `item.updated` and newer item kinds are ignored.

Differences from claude that matter:
- **Prompt is positional argv, not stdin.** Pass after `--`.
- **No incremental text deltas.** The agent message arrives as a
  single fully-formed `item.completed`, not as streaming tokens.
- **`cached_input_tokens` is a SUBSET of `input_tokens`** (not
  additive like claude's cache_creation/cache_read split). Codex
  reports cache hits in-place; the codex driver passes through
  rather than folding.
- **Resume is a subcommand**, not a flag. `codex exec ... resume
  <thread_id> "<prompt>"`. Global flags come before `resume`; the
  prompt comes after.

Security: `--dangerously-bypass-approvals-and-sandbox` is hardcoded
for the same reason claude's `--permission-mode bypassPermissions`
is — non-TTY subprocess can't show the interactive permission UI.
codex's `--cd <workspace_dir>` pins its root but does not constrain
absolute-path reach. Treat `spawn_subagent(backend: "codex", ...)`
as equivalent to running `codex exec ...` in a shell on the host.

## codex — config

```jsonc
"external_agents": {
  "codex": {
    "enabled": false,        // operator must opt in explicitly
    "binary_path": null      // null falls back to PATH lookup
  }
}
```

Same opt-in model as claude — `enabled: false` (the default)
means boot skips this kind entirely.

## gemini — invocation

Same shape as claude/codex, a third wire protocol. Built on
`gemini --output-format stream-json` (the Google Gemini CLI's
non-interactive line-delimited-JSON mode).

```
gemini --output-format stream-json
       --yolo
       --skip-trust
       [--model <name>]
       [--resume <session_id>]
       -p "<prompt>"
```

Event mapping (vs claude's `stream-json` / codex's JSONL):
- `{"type":"init","session_id":…}` → emit `ResumeKey(session_id)` on a
  fresh session (gemini calls its session uuid a `session_id`, like
  claude).
- `{"type":"message","role":"assistant","content":…,"delta":true}` →
  assistant text arrives as incremental deltas, so they're
  **accumulated** (claude/codex send whole turns). Consecutive deltas
  are grouped into one `Intermediate(Assistant text)` row, flushed when
  a tool event interrupts or at stream end, so a token-streamed answer
  doesn't fragment the transcript.
- `{"type":"message","role":"user",…}` → the prompt echo; skipped (the
  spawn router already persisted the task).
- `{"type":"tool_use","tool_name":…,"tool_id":…,"parameters":…}` →
  `Intermediate(Assistant ToolUse)`, name prefixed `gemini_` so a
  transcript reader can't confuse it with an baybo-audited tool call.
- `{"type":"tool_result","tool_id":…,"status":…,"output":…}` →
  `Intermediate(Tool ToolResult)`, linked by `tool_id`. Output may be
  absent; the status string stands in.
- `{"type":"result","status":"success","stats":{…}}` → emit `Usage` +
  `FinalContent`. The result event carries **no** final answer text —
  `FinalContent` is the accumulated assistant deltas.
- `{"type":"result","status":"error","error":{message}}` → terminal
  error.
- Unknown event kinds are ignored.

Differences from claude/codex that matter:
- **Prompt is the `-p <prompt>` flag**, a single argv value (not stdin
  like claude, not a positional after `--` like codex).
- **`--skip-trust` is required alongside `--yolo`.** In an untrusted
  folder gemini silently downgrades `--yolo` to interactive approval —
  which a non-TTY subprocess can never satisfy, so the run hangs.
  `--skip-trust` trusts the workspace cwd so YOLO actually takes.
- **`stats.input_tokens` is the TOTAL prompt; `stats.cached` is a
  subset** (OpenAI/Gemini convention) — passed through, not folded
  like claude's disjoint cache buckets.
- **Resume takes the session uuid directly** (`--resume <session_id>`),
  same as claude's `--resume`.

Security: `--yolo --skip-trust` is hardcoded for the same reason
claude's `bypassPermissions` / codex's
`--dangerously-bypass-approvals-and-sandbox` are — a non-TTY
subprocess can't show the interactive approval UI. The workspace cwd
pins gemini's default working area but does not constrain its
absolute-path reach. Treat `spawn_subagent(backend: "gemini", ...)` as
equivalent to running `gemini --yolo ...` in a shell on the host.

## gemini — config

```jsonc
"external_agents": {
  "gemini": {
    "enabled": false,        // operator must opt in explicitly
    "binary_path": null      // null falls back to PATH lookup
  }
}
```

Same opt-in model as claude/codex — `enabled: false` (the default)
means boot skips this kind entirely.

## Picking a default

When multiple external agents are **enabled**,
`external_agents.default_external_agent` must be set to one of them.
Today this is an operator-visible designation — the spawn protocol
still requires an explicit `backend` value — but a future shorthand
may resolve it.

## CLI commands

- `baybo external-agent status` — show each kind's configured binary
  path and an offline re-probe result (was the binary found and
  does `--version` execute?). Pure read; the running daemon's
  registry isn't affected.
- `baybo external-agent setup` — interactive wizard: single-select
  one kind, prompt binary path (empty = PATH lookup), run the
  probe, then persist the **resolved absolute path** to
  `external_agents.<kind>.binary_path` on success. Even an empty
  answer records the concrete location PATH resolved to, so the
  gateway service (different cwd, possibly a narrower PATH) pins
  the same binary instead of re-walking PATH at boot. If the write
  would leave multiple kinds configured without a default, the
  wizard also prompts for `default_external_agent`. Requires a
  gateway restart to take effect (the command prints a restart hint;
  it does not restart the gateway itself).
- `baybo external-agent disable` — interactive multi-select: check the
  currently-enabled kinds to turn off and set each
  `external_agents.<kind>.enabled = false`. If a disabled kind was
  `default_external_agent`, the default is re-resolved (cleared when
  ≤1 kind remains, else the wizard prompts for a new one). Each
  recorded `binary_path` is left intact for an easy re-enable. When
  nothing is enabled it's a no-op success — the feature is already
  off. Requires a gateway restart to take effect (the command prints
  a restart hint; it does not restart the gateway itself).
- `baybo external-agent default` — interactive picker that sets
  `external_agents.default_external_agent` to one of the
  currently-enabled kinds. Errors when nothing is enabled. The
  default is an operator-facing designation (the spawn protocol
  still requires an explicit `backend`), so it only matters once
  more than one kind is enabled; nothing in the boot/spawn path
  reads it yet, so **no gateway restart** is needed.
- `baybo setup` (quick and full modes) probes every kind on PATH
  after the other setup steps and shows the detected ones in a single
  multi-select (already-enabled kinds pre-checked; on a fresh install
  everything detected is pre-checked). If multiple end up enabled,
  prompts for `default_external_agent` automatically.

## Adding a new external agent

1. Add a variant to `baybo_model::ExternalAgentKind`.
2. Write a new module under `crates/agent/src/external_agent/`
   implementing the `ExternalAgent` trait. Emit `ResumeKey` if the
   underlying tool supports continuation; otherwise just emit
   `FinalContent` (and a parent-side `resume_session_id` for that
   agent will return "doesn't support resume").
3. Add a config struct field in
   `crates/config/src/external_agents.rs` and extend `boot_entries()`
   (required — boot only probes config-backed kinds).
4. Extend the `match kind` in `build_registry`
   (`crates/agent/src/external_agent/mod.rs`) to call the new agent's
   `probe_and_build`; `crates/baybo/src/runtime.rs` needs no change.

That's it — no spawn-protocol changes, no LLM-pool / agent-loop
plumbing, no need to touch the rig-shape `AnyCompletionModel`.
