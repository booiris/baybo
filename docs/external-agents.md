# External Agents

## What

An **external agent** is a subagent backend whose work is delegated to a
subprocess (or other out-of-process driver) instead of running on an
in-process aura `AgentActor`. Two are registered today:

- **`claude`** drives `claude` (Anthropic Claude Code) — billed
  against the operator's Claude Code Max/Pro subscription.
- **`codex`** drives `codex` (OpenAI Codex CLI) — billed against the
  operator's ChatGPT subscription.

Both let an aura subagent's task be handled by the external agent's
own autonomous loop without spending per-token API credit.

Where the LLM crate handles "send messages, get back text" for HTTP
providers, external agents handle "delegate a task, get back a final
result" for binary-backed agents. They're a different shape, not a
different flavor of LLM:

- **Request-response, not conversational.** Each `run()` invocation
  drives the agent through one task and emits a stream of events
  terminating in `FinalContent`. There's no mailbox you keep poking;
  follow-ups happen via a fresh spawn with `resume_session_id`.
- **Autonomous internal tool loop.** claude makes its own
  `Bash`/`Read`/`Write`/`WebFetch` calls. aura's `sandbox` /
  `sensitive_paths` / `approval gate` do **not** apply to those tool
  calls — see the security note below.
- **External-side state.** Resume continuity lives in the agent's own
  session store (e.g. claude's local session uuid), exposed via a
  `resume_key` event that aura persists onto the child Session.

## Where things live

- `aura_model::SubagentBackend` — `Aura { llm: Option<String> } |
  External { kind: ExternalAgentKind }`. Carried on
  `SubagentSpawnRequest.backend`.
- `aura_model::ExternalAgentKind` — discriminator enum: `Claude`,
  `Codex`. Carried on `SubagentBackendTag::External { external_kind,
  workspace_dir, resume_key }`.
- `aura_model::SubagentBackendKind` — discriminator-only view of
  `SubagentBackendTag` for runtime decisions that don't care about
  per-instance state (resume validation, error labels, dispatch).
- `aura_agent::external_agent::ExternalAgent` — async trait with one
  method: `run(request) -> Result<Stream<ExternalAgentEvent>>`.
- `aura_agent::external_agent::ExternalAgentRegistry` — built at boot
  in `src/runtime.rs`; the spawn router looks impls up by kind.
- `aura_agent::external_agent::claude_cli::ClaudeCliAgent` — concrete
  impl for `claude -p` (subprocess + stream-json NDJSON parser +
  ETXTBSY-retrying existence check).
- `aura_agent::actor::router::system_spawn::subagent` — branches on
  `request.backend`: `Aura` takes the existing `AgentActor` spawn
  path; `External` looks up the agent, builds a fresh child Session
  (or loads an existing one on resume), runs the agent, persists any
  emitted resume_key, returns a `SubagentResult`.

## Spawn protocol

```jsonc
spawn_subagent({
  "task_description": "...",
  "must_include_context": ["..."],
  "timeout_secs": 600,

  // Backend selection:
  "backend": "claude",         // or "aura" (default)
  "llm": "fast",                   // only valid when backend = "aura"

  // Optional:
  "workspace_name": "...",         // human-readable working-dir slug
  "resume_session_id": "child-..." // continue a prior subagent
})
```

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
   in the same on-disk dir picked at genesis, regardless of any
   `workspace_name` the parent LLM might supply on the resume call.
   (The spawn-tool parser rejects `workspace_name` combined with
   `resume_session_id` to make this explicit.)

Continuity survives aura restarts because the child Session row is
durable.

The same `resume_session_id` field also works for the Aura backend:
the spawn router loads the existing child Session, the freshly-spawned
one-shot `AgentActor` hydrates the prior transcript via
`restore_transcript_from_store()`, and the new task lands on top of
the restored context.

**Backend identity is durable.** At child-session creation the spawn
router stamps `Session.state.subagent_backend` with the backend's
identity tag — for External, this includes the resolved
`workspace_dir` and `resume_key: None`. Resume validation reads that
field and demands matching identity. Three rejection paths:

1. Backend mismatch — Aura cannot be resumed as External (or vice
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

## claude — security note

`claude -p` runs with `--permission-mode bypassPermissions`
hardcoded. claude's interactive permission prompts can't reach
aura's non-TTY subprocess; bypass means every file edit, bash
command, and network call claude decides to make happens with no
further confirmation. **aura's `sandbox` / `sensitive_paths` /
`approval gate` do NOT apply to claude's internal tool calls.** The
per-spawn `--add-dir <workspace_dir>` only constrains claude's
*default* working area, not its absolute-path reach. Treat
`spawn_subagent(backend: "claude", ...)` as equivalent to
running `claude -p '...'` in a shell on the aura host.

## claude — where it runs

`<workspace_root>/work/claude/<dir>/`, where `<dir>` is either
the `workspace_name` slug picked by the parent LLM in the
`spawn_subagent` call (sanitised kebab-case ASCII, capped at 32
chars), or the child session_id when no name was supplied. The dir
is resolved exactly once — at genesis — and persisted on
`SubagentBackendTag::External.workspace_dir`. Resume calls read it
back from the tag; they do not re-derive from the resume-time
request. Two spawns with the same `workspace_name` deliberately
share the same on-disk dir — pass the same name when a sibling
subagent should pick up where another left off. The dir is created
on demand and never auto-cleaned (matches CLAUDE.md's "session data
is core data — never deleted" rule); operators who want to reclaim
disk delete the dir manually.

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
`aura external-agent setup`, `aura setup`, or by editing
`aura.json`) to make the LLM able to invoke that backend.

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
  accumulate as final assistant text.
- `turn.completed { usage }` → emit `Usage` + `FinalContent`.
- `turn.failed` / top-level `error` → terminal error.
- `reasoning` / tool-call items / `item.started` / `item.updated` are
  ignored (codex's autonomous tool loop is opaque to aura).

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

## Picking a default

When multiple external agents are **enabled**,
`external_agents.default_external_agent` must be set to one of them.
Today this is an operator-visible designation — the spawn protocol
still requires an explicit `backend` value — but a future shorthand
may resolve it.

## CLI commands

- `aura external-agent status` — show each kind's configured binary
  path and an offline re-probe result (was the binary found and
  does `--version` execute?). Pure read; the running daemon's
  registry isn't affected.
- `aura external-agent setup` — interactive wizard: single-select
  one kind, prompt binary path (empty = PATH lookup), run the
  probe, persist on success. If the write would leave multiple
  kinds configured without a default, the wizard also prompts for
  `default_external_agent`. Restarts the gateway to take effect.
- `aura setup` (quick mode) probes both kinds on PATH after the
  other setup steps. Each detected binary triggers a y/n confirm;
  the operator picks which to enable. If multiple end up enabled,
  prompts for `default_external_agent` automatically.

## Adding a new external agent

1. Add a variant to `aura_model::ExternalAgentKind`.
2. Write a new module under `crates/agent/src/external_agent/`
   implementing the `ExternalAgent` trait. Emit `ResumeKey` if the
   underlying tool supports continuation; otherwise just emit
   `FinalContent` (and a parent-side `resume_session_id` for that
   agent will return "doesn't support resume").
3. Optionally add a config section under
   `crates/config/src/external_agents.rs`.
4. Register in `src/runtime.rs` alongside the `claude`
   registration.

That's it — no spawn-protocol changes, no LLM-pool / agent-loop
plumbing, no need to touch the rig-shape `AnyCompletionModel`.
