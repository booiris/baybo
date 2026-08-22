# trace - Step / Span Types, Store, and Lifecycle Recorder

## Overview

The `trace` crate is the home for the four-tier observability model: domain types (`Step`, `StepKind`, `Span`, `SpanKind`, `SpanEvent`, `SpanEventKind`, `ToolEventPayload`, `LlmToolCallRecord`, `ToolCallOrigin`), the row conversions that persist them, and the `SpanRecorder` lifecycle facade (with its `TraceEvent` / `TraceEventStream` broadcast bus).

The `TraceStore` trait itself lives in the `baybo-store` ports crate and trades in row DTOs — `StepRow` / `SpanRow` / `SpanEventRow`, each a queryable key plus the serialized entity in a `data` field. This crate owns the `Step::to_row` / `Step::from_row` (and `Span` / `SpanEvent`) conversions and converts at the recorder boundary, so the rich types and the recorder logic stay here while the trait sits in a leaf crate every store consumer can reach. `baybo-storage` provides the sqlite implementation, shuttling rows without depending on `baybo-trace` (it converts in its tests only). `impl From<baybo_store::StorageError> for TraceError` bridges errors at the call sites.

Trace answers **"what exactly did this operation do"** by recording sanitized inputs, results, latency, and execution provenance. Its difference from `turn` is: **Turn manages state, Trace manages content.**

The hierarchy is `Session > Turn > Step > Span (+ SpanEvent)` — see `session.md` for the top two layers and `turn.md` for state-machine details. Trace covers the bottom two plus events.

## Design Decisions

### Step is the agent-loop iteration unit; Span is an OTel-compatible atomic action

A `Step` is one iteration of the agent loop. A `Span` is one atomic action with a start/end window — naming aligns with OpenTelemetry so this trace can export to standard collectors without a translation layer. A `SpanEvent` is a zero-duration marker on a Span (sanitize hit, approval decision).

Steps cannot nest. Spans within a Step can be parallel (siblings sharing a `parallel_group: ParallelGroup`; the field is `Option<ParallelGroup>`, and `None` means strictly sequential) but do not nest either. This is a fixed three-layer fan-out — not a free-form tree.

### Closed strong-typed enums

`StepKind` and `SpanKind` are closed enums; each variant carries its own typed input/output/provenance fields. There is no superset struct with `Option` fields, no `serde_json::Value` payload as a backdoor. New kinds are added by extending the enum, never by string tagging. (`SpanKind::ToolCall.params` and `output` use `serde_json::Value` because tool schemas are dynamic — that is the boundary, and it is the only one.)

```rust
pub enum StepKind {
    LlmIteration,
    Compression,
    MemoryRecall,
    MemoryWrite,
    SkillSelection,
    ProgressObserver,   // out-of-band turn-progress summary LLM call
    TitleGeneration,    // conversation-title generation
}

pub enum SpanKind {
    LlmCall {
        begin: LlmCallBegin,            // model_id, provider, provider_config_hash, input_messages, temperature, tools
        result: Option<LlmCallResult>,  // output_content, thinking, tool_calls, *_tokens (None while Pending)
    },
    ToolCall {
        begin: ToolCallBegin,           // tool_name, tool_artifact_hash, triggered_by, params
        result: Option<ToolCallResult>, // Inline/Persisted output, success (None while Pending)
    },
}
```

### Step / Span cardinality rules

- One agent-loop iteration produces **exactly one** `Step` of kind `LlmIteration` containing ≥1 LLM `Span` (one per attempt — a transient-error retry opens a new `LlmCall` span in the same Step; tool spans pair back to the last attempt) + 0..N tool `Span`s. A pure-response iteration (no tool calls) still opens a Step containing one `LlmCall` span.
- Parallel tool calls are **sibling spans** under the same Step with the same `parallel_group: ParallelGroup`. Their time windows may overlap.
- LLM ↔ tool pairing is by `ToolCallOrigin { llm_span_id, tool_use_id }`, not by tree structure. Tool spans are direct children of the Step.
- `Compression`, `MemoryRecall`, `MemoryWrite`, `SkillSelection`, `ProgressObserver`, and `TitleGeneration` are first-class Step kinds, not events on an LLM step.
- `Compression` carries `CompressionTrigger`: **why** it ran (`Threshold` / `Forced`) — the token threshold tripped, or the user typed `/compact`. There is no "how" field, because a live summary is the only way a transcript is ever shortened.
- Every `Compression` step therefore wraps at least one `LlmCall` span — two when a transient failure was retried, both under the one step. A compaction whose summariser call failed applies **nothing** (no truncate fallback exists), so its step closes `Failed` with the provider's reason rather than `Ok`, and the user is told.

### Provenance lives on Span variants, not on Step

Each `SpanKind` variant carries the provenance fields that apply to it (`model_id` and `provider` on `LlmCall`, `tool_artifact_hash` on `ToolCall`). Step is a pure container with no provenance.

### SpanEvent is sanitize / approval audit, not control flow

```rust
pub enum SpanEventKind {
    SanitizeHit { hits_count, kinds: Vec<SecretKind>, placeholder_ids: Vec<PlaceholderId> },
    Approval { decision: ApprovalDecision, resource: ResourceAccess },
    ToolEvent { action: String, payload: ToolEventPayload },
}

pub enum ToolEventPayload {
    Phase { duration_ms: u64 },
    HttpFetch { status: u16, bytes: u64, content_type: Option<String>, body_preview: Option<String> },
    LlmCall { model: String, input: String, output: String },
    ParseFailure { command: String },
}
```

- `SanitizeHit` is emitted **only when sanitize actually modified content**. Misses are not recorded — the trace records what happened, not what ran.
- `Approval` records **every** decision (`Approve`, `ApproveAlways`, `Deny`). The audit trail of "what did the user approve and when" is complete.
- `ToolEvent` is a tool-emitted phase artifact — one per `ToolEventSink::emit` call inside a tool body (a sub-action's elapsed time, an HTTP response summary, a side-LLM round-trip). The agent layer drains the tool's event buffer after execution, sanitizes text payload fields, then emits one `SpanEvent` per entry. `ToolEventPayload` text fields are producer-truncated and still pass through the leak detector before persistence.
- `ParseFailure` records a shell command the destructive-command detector failed to parse with the shell grammar (it fell back to the fail-closed keyword pre-filter), so parser gaps stay visible. `command` is producer-truncated and sanitized before persistence.

### Sanitization constraints

- Record only sanitized payloads — secrets appear only as placeholders
- `placeholder_ids` are kept in `SanitizeHit` so replay can resolve them via `SecretVault`
- Apply uniform sanitization to every `SpanKind` result variant — error paths included

### ToolCall output storage

`ToolCallResult.output` has the same compatibility split as LLM inputs:

- **`Inline(Value)`** keeps the historical bare JSON wire shape (a `{ "type":
  … }`-tagged value). Smaller results stay inline because a reference would cost
  more bytes.
- **`Persisted(PersistedToolCallOutput)`** points at the transcript row whose
  `ToolResult.content` already carries the model-facing result, keyed by the
  call's `tool_use_id` (a *begin-time* fact). It stores only that id plus the
  out-of-band media blocks a `WithAttachments` / `MultiModalText` result carries
  beside its text (`attachments` / `llm_images` — small `BlobRef` pointers that
  never enter the `ToolResult` text). Resolution finds the block by `tool_use_id`
  and returns its content verbatim — the raw wrapped, capped envelope the model
  saw — so a text/json/error result resolves to that string and a media result
  re-wraps it into the tagged object to keep the blob list.

Because `tool_use_id` is known when the span opens, the tool span closes during
execution — no deferred close, no waiting for the transcript ordinal. The write
chooses `Persisted` only when its serialized pointer is smaller than the capped
inline value. The pointer is written before `AgentLoop` appends the transcript
row, so a rare append failure leaves it unresolvable rather than dangling
silently: replay surfaces a visible `trace_reconstruction_error` for a
`tool_use_id` with no matching `ToolResult`.

The model-facing payload is still capped at
`baybo_context::prompts::tool_output::MAX_TOOL_OUTPUT_BYTES` and any full value
is content-addressed in the tool-spill directory; `output_truncated_from`
retains the uncapped serialized length. `QueryApi::replay` resolves references
server-side, while the per-turn web endpoint leaves them compact and
`resolveToolCallOutput` resolves them against the overview's one transcript log.

### The tool set an LlmCall offered, by content address

`LlmCallBegin.tools` records **what the model was allowed to call**, the other
half of "what did the LLM see" beside `input_messages`. It is an
`Option<LlmToolSetRef> { hash: ToolSetHash, count: usize }` — a reference, for
the same reason `LlmCallInputs::Persisted` is one — the list runs to tens of KB
of JSON schema, and inlining it per call would make the schemas the largest
thing in the `spans` table.

A session's list is *mostly* stable, which is what keeps prompt caching alive,
but it is not stable **by construction** and the hash is the place that shows
it: `tool_definitions_for_session` is called afresh per request, and the MCP
reconciler connects and drops servers underneath live sessions. So one session
can legitimately record more than one hash, and a hash changing mid-session is
a real event worth reading — the model was handed a different set — not a bug
in the recorder.

The definitions live in `llm_tool_sets(hash TEXT PRIMARY KEY, data TEXT)`,
keyed by the SHA-256 of their own serialized body — so writes are
`INSERT OR IGNORE`, rows are immutable, and the same set never stores twice.
`SpanRecorder::record_tool_set` serializes, hashes, and memoises the hashes it
has already written, which collapses one store write per LLM call into one per
distinct set. `None` means the call genuinely offered no tools (compression,
title generation, the progress observer all send an empty list) or predates the
field.

`QueryApi::load_tool_set` resolves a hash, and `GET /v1/traces/tool-sets/{hash}`
exposes it. Deliberately **not** folded into the per-turn tree: every span in a
turn names the same set, so inlining it there would re-ship those tens of KB per
span. The web viewer fetches lazily when the span detail's Tools tab is opened
and caches by hash, which makes it one request per page visit and zero for a
reader who never looks.

### Single-table persistence

Step and Span lifecycle writes go to the canonical tables (`steps`, `spans`, `span_events`, plus the content-addressed `llm_tool_sets` above). Each row stores the entity as a single JSON `data` blob; queryable fields (`turn_id`, `step_id`, `started_at`, `ended_at`) surface as `GENERATED ALWAYS AS (json_extract(...)) VIRTUAL` columns that SQLite keeps in lockstep with `data` automatically. There is no two-side write contract — adding a new field is a serde change in `baybo-trace`, no schema migration. New indexed lookups need a new generated column; that is the only schema change vector.

The earlier two-layer WAL (`trace_events` table mirroring every begin/end) was removed once it became clear no reader consumed it: recovery scans `spans` directly, and there is no replay / OTel-export path yet that would benefit from the append-only log. If one lands later, the WAL can come back together with its consumer.

### LlmCall input storage: Inline vs Persisted

An `LlmCall` span records what the model saw in `begin.input_messages`
(`LlmCallInputs`), which has two shapes:

- **`Inline(Vec<ChatMessage>)`** — messages embedded directly. Used only when the
  input is genuinely not in `session_messages`. Self-contained (cannot desync) but
  costs the full message bytes per span.
- **`Persisted { last_ordinal, prefix_len, suffix }`** — a *reference* to the
  `session_messages` active-as-of-`last_ordinal` slice (the ordinal log in
  [storage.md](storage.md)). The main agent, compression, and the progress
  observer all use it. It exists to avoid embedding the prefix: the main agent
  would otherwise re-clone a growing prefix every turn (O(N²) over session
  length), and compression / observer would re-embed the whole summarised window
  on every fire. `suffix` carries the framing that is *not* a `session_messages`
  row — a compression instruction, the observer prompt, the background-summary
  sub-loop's own turns — appended verbatim after the reconstructed prefix so the
  rebuilt view equals exactly what the LLM saw.

Hydration (`QueryApi::replay`, and the web client's `hydratePersistedInput`)
collapses every `Persisted` back to `Inline` for consumers: it reconstructs the
prefix with the "active as of N" filter and appends `suffix`.

**Per-session resolution.** Background compression runs as an in-actor detached
step on the parent's own `AgentLoop`, so its `StepKind::Compression` / `LlmCall`
spans live directly under the **parent** session and their `last_ordinal` /
`prefix_len` are recorded against the parent's own transcript. Hydration reads
each session's own log: `replay` passes the replayed `session_id` straight to
`hydrate_persisted_trace_data`, while `load_trace_overview` returns that same
session's message log and `load_turn_trace` leaves `Persisted` pointers intact for
the web client to resolve via `hydratePersistedInput` against it. Every session
resolves to itself.

**`prefix_len` is a self-validating tripwire.** The reference points into mutable
derived state (`superseded_by` bookkeeping), so a `superseded_by` bug, a deleted
row, or a read/write-filter divergence could silently rehydrate the wrong slice.
`prefix_len` records how many prefix messages the writer expected; hydration
compares it against the reconstructed count and, on mismatch, prepends a visible
`Role::System` marker (and logs a warning) rather than returning a plausible-but-
wrong input. A `Persisted` marker is only emitted when the count is known, so
every reference is validated — there is no skip path. A hydration *code* bug is
recoverable (the truth lives in durable `session_messages`; `replay` is pure and
re-runnable after the fix) — the tripwire only makes drift loud, not silent. The
write-side / read-side filter equivalence is pinned by a differential test, the
marker path by a negative test.

### Synchronous lifecycle writes

Every Step / Span lifecycle write is awaited inline before execution proceeds —
`SpanRecorder::begin_*` / `end_*` return only once the row is durable — so the
previous span's `end` and the current span's `begin` are always persisted before
an LLM or tool request goes out. There is no deferred/background write path.

### Recovery

`baybo_agent::recovery` closes half-open trace rows left behind by a process or
actor that died. It is not the only closer of a dropped future: a span whose
guard future is dropped while the process lives (a `/stop` abandoning an
in-flight tool call) is closed immediately by `runtime::scope::with_span`'s Drop
guard, and has to be — recovery reaches pending spans only under a step that is
itself unfinished, and that step closes normally on the same unwind.

At boot, `recover_orphaned_traces_and_turns` walks non-terminal turns from the
prior process, closes pending spans/steps at the last observed child activity,
and cancels the turn as `SystemCrash`. It also asks `TraceStore` for unfinished
steps so detached work under already-terminal turns (title generation, progress
observer) is closed without reopening or cancelling the owning turn. While the
process is still alive, `recover_panicked_actor_session` performs the same repair
for the panicked session's active chat turns, using the actor task's crash time
as the close time.

## Constraints

- Depends on `baybo-turn` (for `CancelReason`) and `baybo-model` (for `TurnId`, `SessionId`, `ChatMessage`, `ContentBlock`, `SecretKind`, etc.). No dependency on `baybo-storage`.
- IDs use ULID newtypes (`StepId`, `SpanId`); `SpanEvent` uses a `(span_id, seq)` compound key
- Storage uses columnar schema: `steps` / `spans` / `span_events` (one row per entity) plus `llm_tool_sets` (one row per distinct tool set, keyed by its own digest); the `Turn > Step > Span` parent chain is encoded by foreign keys, not by embedded child lists
- Trace deletes are hard `DELETE FROM` — no `deleted_at` tombstone column (see [storage.md](./storage.md#hard-delete-everywhere-but-cron_jobs))
- `SpanRecorder` holds locks only for short critical sections, never across `await`
- `test_support::MemoryTraceStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `baybo-trace = { workspace = true, features = ["test-support"] }`.

## Collaboration

| Module    | Role                                                                                                                         |
| --------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `turn`    | Turn manages state, Trace manages content; linked via `TurnId`; `partial_artifacts: Vec<SpanId>` references trace spans       |
| `agent`   | Constructs and shares one `SpanRecorder` per session; uses `TurnLifecycle` and `SpanRecorder` together as sibling facades      |
| `store`   | Owns the `TraceStore` trait + its `StepRow` / `SpanRow` / `SpanEventRow` DTOs and `StorageError`; this crate converts rich types ↔ rows |
| `storage` | Provides the sqlite implementation of `TraceStore` (from `baybo-store`), shuttling rows; depends on `baybo-trace` only as a dev-dependency |
| `model`   | Provides `SessionId`, `ChatMessage`, `ContentBlock`, `SecretKind`, `PlaceholderId`, `ApprovalDecision`, `ResourceAccess`       |
