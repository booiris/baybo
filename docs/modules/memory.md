# memory — pluggable long-term memory

## Overview

The `baybo-memory` crate defines a **single pluggable [`Memory`] trait**. The
system knows memory only through one `Arc<dyn Memory>` slot (not a many-registry
like tools/channels): at most one implementation is registered at startup. The
trait is intentionally thin and **storage-opaque** — an implementation owns its
own persistence (libsql, a vector DB, an external service) and receives its LLM
handle and config in its own constructor.

The core ships the trait, its value types (`MemoryContext`, `RecalledMemory`,
`MemoryError`), and a **`NoopMemory`** reference default. **No real backend ships
yet** — the runtime wires `None` (the inert no-op path), so in production today
nothing is recalled, written, or billed. The plumbing below activates the moment
a real `Arc<dyn Memory>` is registered.

This supersedes the previous CRUD `MemoryManager` facade (now removed, along with
`MemoryStore`/`MemoryEntry`/`MemoryCategory`, the libsql impl, the `/v1/memory`
admin REST, and the `memories` table — dropped by migration). The earlier
heuristic `recall` + `maybe_store` pipeline was retired because it (1) recalled
by arbitrary substring match, (2) treated entire assistant outputs as memorable,
and (3) re-injected snapshots as `Role::System` messages that polluted every
later turn. Those three failure modes are now **hard constraints** (below), not a
reason to forbid the whole shape.

## The trait

```rust
// crates/memory/src/lib.rs
pub struct RecalledMemory { pub content: String }

// Carries the real (user, session, job) + the session's agent_id (partition
// key, see "Partitioning by agent") + the trace recorder + the enclosing
// memory step. `scoped_llm_call(begin, body)` opens an LlmCall span under that
// step and hands `body` an Attribution bound to it, for billed sub-calls.
pub struct MemoryContext { /* user_id, agent_id, session_id, job_id, recorder, step */ }

#[async_trait]
pub trait Memory: Send + Sync {
    async fn recall(&self, ctx: &MemoryContext, query: &[ContentBlock])
        -> Result<Vec<RecalledMemory>>;
    async fn on_job_complete(&self, ctx: &MemoryContext,
        user_input: &[ContentBlock], final_output: &[ContentBlock]) -> Result<()>;
    async fn on_session_end(&self, ctx: &MemoryContext,
        transcript: &[ChatMessage]) -> Result<()>;
    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> { Vec::new() }
}
```

- **`recall`** — synchronous query, on the critical path (job start + each
  interjection). De-duplication against already-surfaced memories is **internal
  to the impl** (keyed off `ctx`; one impl is a process singleton, so that state
  survives actor reap/rehydration for free). The core injects exactly what is
  returned.
- **`on_job_complete` / `on_session_end`** — fire-and-forget lifecycle events
  (the read/write asymmetry is intentional; the verb is **not** "sync", which
  would imply bidirectional reconciliation). `on_job_complete` sees one finished
  exchange (`user_input` includes mid-turn interjections); `on_session_end` sees
  the full durable transcript at idle-timeout.
- **`tools`** — the model's "explicit signal" path, coexisting with the
  automatic recall/write path. Registered statically at startup.

## Clients & billing

The implementation holds the **unbound** `Arc<BillableLlm>`, constructor-injected.
The core hands each call a `MemoryContext` carrying the real `(user, session,
job)` + the trace recorder + the enclosing `MemoryRecall` / `MemoryWrite` step;
the impl binds its handle per billed sub-call via
`MemoryContext::scoped_llm_call` (below), so spend bills to the **real**
user/session under a real span (mirrors `compression.rs`, not
`Attribution::system`).

**Span attribution.** `cost_records.span_id` is written by `record_call` keyed
off `attribution.span_id`, so a billed sub-call needs a *real* span or its cost
row is orphaned. `MemoryContext::scoped_llm_call(begin, body)` provides one: it
opens an `LlmCall` span **under the memory step**, hands `body` an `Attribution`
bound to that span (built from the real user/session/job + the span id), and
closes the span with the call's token usage. The impl binds its `BillableLlm`
with that attribution and makes the call — so the cost row lands on a recorded
span, attributed to the real user/session/job. The impl never constructs a bare
`Attribution`, so it can't bill against an orphaned id.

## Recall injection (enforces hard constraint #3)

Recalled memories enter the prompt as a **persisted, framed** block — never
`Role::System`. The path mirrors the user-interjection pattern:

- `MessageSource::RecalledMemory` (`baybo-model`) + `ChatMessage::recalled_memory`.
- `baybo_context::prompts::recalled_memory` — a `<recalled_memory>` envelope,
  re-derived wire-only in `ContextManager::messages_for_llm` (via
  `frame_recalled_memories`, alongside `frame_interjections` — both delegate to
  one `frame_source_runs` helper). The budget counts the framed size.
- Persisted once per recall (`append_recalled_memory`); rides the transcript
  until compression folds it. Hidden from the chat bubble surface
  (`from_user() == false`), so it never renders as a user turn.
- The core does **no** dedup — it injects exactly what `recall` returns.

## Partitioning by agent

`MemoryContext.agent_id` is the partition key threaded through every hook call: the agent loop sets it from `SessionState::agent_id_or_builtin()` — the session's bound [agent profile](agent-profiles.md#session-binding) id, or `BUILTIN_AGENT_PROFILE_ID` (`"baybo"`) for an unbound session. Agent A's session never recalls or writes into Agent B's partition, even though both share one Mem0 project / OpenViking deployment. The builtin id equals the backends' default agent namespace, so unbound sessions and pre-binding memories share one partition; only a new custom agent's partition starts empty.

- **mem0**: `recall`'s `POST /v2/memories/search/` filters on `{AND: [{user_id}, {agent_id}]}`; `on_job_complete`'s `POST /v1/memories/` write carries `agent_id` alongside `user_id`.
- **OpenViking**: every request carries `X-OpenViking-Agent: <agent_id>`; there is no per-call override.

Memory tools (`mem0_*`, `viking_*`) get the same scoping through `ToolContext.agent_id` (`Option<AgentProfileId>`; `None` for an unbound session falls back to the backend's own default constant — `mem0::DEFAULT_AGENT_ID` / `openviking::DEFAULT_AGENT`, both `BUILTIN_AGENT_PROFILE_ID`). No tool of either backend exposes an `agentId`-style override — the agent namespace always tracks the calling session, including for `mem0_delete{all: true}` and `mem0_list`, a deliberately narrow blast radius for the destructive path. Partition isolation is the invariant; a cross-agent operation is an operator action (e.g. via the Mem0 dashboard), never a tool call.

## Flow & hook points

Two gates run independently:

- **`memory_recall_query(&JobInput)`** — per-job gate for the read/write hooks
  on a turn. Exhaustive match: returns the query for `UserChat` / `Cron`, `None`
  for `System`, `Spawned` (subagent), and `SubagentNotification` (no direct
  user input → would pollute / double-write).
- **`should_fire_session_end(&Session)`** — session-level gate for the
  shutdown hook. Returns true for root `User` / `Cron` sessions; false for
  `Subagent` and `System`-triggered actors (they send `ActorStop` too but are
  not user-session endings).

The agent loop (`agent_loop.rs`) owns the first three hooks; the actor's
`AgentMessage::ActorStop` handler (`actor/mod.rs`) owns the fourth.

1. **Recall — inline, job start.** After the user message is in context and
   before the first LLM call, `recall_and_inject` opens a `MemoryRecall` step,
   mints `Attribution`, calls `recall`, and injects each result as a
   `RecalledMemory` row. Recall failure is logged and swallowed — never fails
   the turn.
2. **Recall — inline, interjection.** Each mid-turn interjection drained at a
   tool boundary is recalled against too, and folded into the job's input.
3. **`on_job_complete` — background, job end.** At `IterationOutcome::Final`,
   `spawn_job_complete_write` detaches a task (so the actor returns the answer
   without waiting) that opens a `MemoryWrite` step and calls `on_job_complete`.
   **Only on a clean `Final`** — a max-iterations, cancelled, or errored turn
   writes nothing (those paths return before this point), so memory only ever
   sees completed exchanges.
4. **`on_session_end` — background, actor shutdown.** When the actor processes
   `AgentMessage::ActorStop` (idle reap, supervised shutdown, …),
   `spawn_session_end_write` detaches a task on the runtime root **before**
   cancelling `actor_token`, so the write survives the actor's teardown. The
   task loads the FULL durable transcript via `SessionManager::history`
   (the in-memory view may have been compressed), opens a `MemoryWrite` step,
   mints a synthetic `JobId` for trace/cost keying, and calls
   `on_session_end`. The session-level gate skips subagent / maintenance /
   system actors (whose `ActorStop` is not a user-session ending). A missing
   session row or empty transcript skips silently.

With `memory == None` every hook above is skipped entirely — no trace step is
opened and nothing is billed, so the no-op path is genuinely inert.

## Config

`MemoryConfig` on `BayboConfig` (`crates/config/src/memory.rs`): typed
core-wiring knobs (`enabled`, `llm` entry name) **plus** an opaque
`extra: serde_json::Value` passed through verbatim to the plug-in. The `extra`
bag is a deliberate, documented exception to the "typed over `Value`" rule —
plug-in config is genuinely opaque to the core. Memory config is **not**
hot-reloadable (`reload.rs` classifies it non-hot). Embedding provider/model
settings, if a backend needs them, ride in `extra` rather than a typed core
field — the core has no opinion on whether a backend embeds at all.

## Hard constraints (carried forward from the retired pipeline)

1. **No substring recall.** Recall is LLM- (or embedding-) judged relevance,
   never substring match against free-form text.
2. **No whole-output storage.** The write path must judge salience, never treat
   an entire assistant output as a memory.
3. **No `Role::System` re-injection.** Recalled memories use the persisted,
   wire-framed `RecalledMemory` path only — the core enforces this structurally
   (it is the single injection path).

These are the implementation's contract.

## Backends

Two real backends ship in the crate, selected at startup via
[`MemoryConfig.provider`](../../crates/config/src/memory.rs) (`mem0` /
`openviking`); `noop` (the default) keeps the inert path. Both delegate
extraction to their respective servers, so neither uses
[`MemoryConfig.llm`](../../crates/config/src/memory.rs) — the field stays on
the typed config for future backends.

### `mem0` (`baybo_memory::mem0`)

Hosted SaaS via the Mem0 Platform REST API. Per-user scope comes from the
caller's `user_id` at every call; `agent_id` (see [Partitioning by
agent](#partitioning-by-agent)) scopes both hook calls and tool calls to the
session's bound agent (`ToolContext.agent_id`) — not overridable by any
param. Tool reads separately accept an optional `scope: "session"` that
narrows to the current session via Mem0's `run_id` (sourced from
`ToolContext::session_id`).

| Hook | Behaviour |
| --- | --- |
| `recall` | `POST /v2/memories/search/` with `{query, filters: {AND: [{user_id}, {agent_id}]}, rerank, top_k}`; returns the `memory` text verbatim. |
| `on_job_complete` | `POST /v1/memories/` with `{messages: [{user,assistant}], user_id, agent_id}`; the Mem0 server runs LLM-based fact extraction. |
| `on_session_end` | No-op (Mem0 has no session concept; extraction is per-`add`). |
| `tools()` | Eight `mem0_*` tools — the model's explicit-signal path (see below). |

The tool surface mirrors the Mem0 `openclaw` plugin (each `mem0_`-prefixed),
mapped onto Mem0 REST endpoints. Unlike `on_job_complete`, `mem0_add` stores
verbatim (`infer: false`) — the model already decided what is worth keeping.

| Tool | Endpoint | Purpose |
| --- | --- | --- |
| `mem0_search` | `POST /v2/memories/search/` | Semantic search; optional `scope` / `categories` / advanced `filters`. |
| `mem0_add` | `POST /v1/memories/` (`infer: false`) | Store fact(s) verbatim; `category` / `importance` / `metadata`; `longTerm: false` → session-scoped. |
| `mem0_get` | `GET /v1/memories/{id}/` | Fetch one memory by id. |
| `mem0_list` | `POST /v2/memories/` | List the user's memories in the session's agent partition (paginated). |
| `mem0_update` | `PUT /v1/memories/{id}/` | Replace a memory's text in place. |
| `mem0_delete` | `DELETE /v1/memories/{id}/` or `?user_id=` | Delete by id, search-and-delete by `query`, or `all: true` + `confirm: true` — `query`/`all` variants scope to the session's agent partition. |
| `mem0_event_list` | `GET /v1/events/` | List recent background processing events. |
| `mem0_event_status` | `GET /v1/event/{id}/` | Status / latency / results of one event. |

Failure handling: 5-failure / 120 s circuit breaker shared by every API call
(pauses API calls after sustained outages). Recall failures are
swallowed and logged at `warn`. API key resolution: vault entry
`user_env.<api_key_name>` (managed via `baybo secret add <name>`) → process
env `<api_key_name>`. `<name>` defaults to `MEM0_API_KEY` when
`api_key_name` is unset in config.

### `openviking` (`baybo_memory::openviking`)

Self-hosted context database. Baybo `SessionId` maps 1:1 to the OpenViking
session id; `X-OpenViking-Account` (config) carries deployment identity,
`X-OpenViking-Agent` carries `MemoryContext::agent_id()` (the session's bound
agent, `"baybo"` for an unbound one — see [Partitioning by
agent](#partitioning-by-agent)), and `X-OpenViking-User` carries
`MemoryContext::user_id()`, all per call.

| Hook | Behaviour |
| --- | --- |
| `recall` | `POST /api/v1/search/find` with `{query, top_k}`; returns `"{abstract} (viking://uri)"`. |
| `on_job_complete` | `POST /api/v1/sessions/{ctx.session_id}/messages` ×2 (user, assistant). |
| `on_session_end` | `POST /api/v1/sessions/{ctx.session_id}/commit` — triggers the 6-category server-side extraction (preferences / entities / events / cases / patterns / profile). Skipped if `transcript.is_empty()`. |
| `tools()` | Four `viking_*` tools (see below). |

The tool surface mirrors the official OpenViking `openclaw-plugin`, each
`viking_`-prefixed:

| Tool | Endpoint(s) | Purpose |
| --- | --- | --- |
| `viking_recall` | `POST /api/v1/search/find` | Search memories; without `targetUri`, queries `viking://user/memories` + `viking://agent/memories` concurrently, then merges / dedups / leaf-filters. |
| `viking_store` | `POST …/messages` + `…/commit` | Write one session message, commit, and poll the extraction task to a memory count. |
| `viking_forget` | `DELETE /api/v1/fs` | Delete by memory URI, or search-and-delete on a strong single match (`is_memory_uri` guards against deleting non-memory paths). |
| `viking_archive_expand` | `GET /api/v1/sessions/{sid}/archives/{id}` | Fetch the original messages from a compressed session archive. |

API key is optional (local dev mode runs unauthenticated). Resolution:
vault entry `user_env.<api_key_name>` (`baybo secret add <name>`) → process
env `<api_key_name>` → empty (unauthenticated). `<name>` defaults to
`OPENVIKING_API_KEY` when `api_key_name` is unset in config. Startup health
probe is `GET /health`; failure logs `warn` and continues.

### Operator CLI

`baybo memory {status, setup, test, disable}` — see
[`docs/cli.md`](../cli.md#baybo-memory). Configure is interactive (provider +
per-field prompts, vault-stash for the API key); memory config is **not**
hot-reload, so `setup` prints a restart hint.

## Deferred

- Operator/GDPR wipe of memory rows: re-add behind a user-triggered command if a
  future backend needs it (no background sweeper — see CLAUDE.md).

## Collaboration

| Module      | Role                                                                            |
| ----------- | ------------------------------------------------------------------------------- |
| `model`     | `MessageSource::RecalledMemory`, `ChatMessage::recalled_memory`, `BUILTIN_AGENT_PROFILE_ID` |
| `llm`       | `Attribution`; `BillableLlm`/`BoundBilledLlm` (memory's billed chat handle)     |
| `tools`     | `Tool` / `ToolManifest` for `Memory::tools()`; `ToolContext.agent_id` defaults memory-tool scoping |
| `context`   | `<recalled_memory>` framing + `append_recalled_memory` + budget                 |
| `agent-profiles` | `SessionState::agent_id_or_builtin()` is the source of `MemoryContext.agent_id` (see [Partitioning by agent](#partitioning-by-agent)) |
| `agent`     | Drives `recall` / `on_job_complete`; `AgentLoopConfig.memory`                   |
| `config`    | `MemoryConfig` (typed knobs + opaque `extra`)                                   |
| `trace`     | `StepKind::MemoryRecall` / `MemoryWrite`                                         |
