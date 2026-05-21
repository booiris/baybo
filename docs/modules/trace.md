# trace - Step / Span Types, Store, and Lifecycle Recorder

## Overview

The `trace` crate is the home for the four-tier observability model: domain types (`Step`, `StepKind`, `Span`, `SpanKind`, `SpanEvent`, `SpanEventKind`, `LlmToolCallRecord`, `ToolCallOrigin`), the row conversions that persist them, and the `SpanRecorder` lifecycle facade (with its `TraceEvent` / `TraceEventStream` broadcast bus).

The `TraceStore` trait itself lives in the `aura-store` ports crate and trades in row DTOs — `StepRow` / `SpanRow` / `SpanEventRow`, each a queryable key plus the serialized entity in a `data` field. This crate owns the `Step::to_row` / `Step::from_row` (and `Span` / `SpanEvent`) conversions and converts at the recorder boundary, so the rich types and the recorder logic stay here while the trait sits in a leaf crate every store consumer can reach. `aura-storage` provides the libsql implementation, shuttling rows without depending on `aura-trace` (it converts in its tests only). `impl From<aura_store::StorageError> for TraceError` bridges errors at the call sites.

Trace answers **"what exactly did this operation do"** by recording sanitized inputs, results, latency, and execution provenance. Its difference from `job` is: **Job manages state, Trace manages content.**

The hierarchy is `Session > Job > Step > Span (+ SpanEvent)` — see `session.md` for the top two layers and `job.md` for state-machine details. Trace covers the bottom two plus events.

## Design Decisions

### Step is the agent-loop iteration unit; Span is an OTel-compatible atomic action

A `Step` is one iteration of the agent loop. A `Span` is one atomic action with a start/end window — naming aligns with OpenTelemetry so this trace can export to standard collectors without a translation layer. A `SpanEvent` is a zero-duration marker on a Span (sanitize hit, approval decision).

Steps cannot nest. Spans within a Step can be parallel (siblings sharing a `parallel_group: GroupId`) but do not nest either. This is a fixed three-layer fan-out — not a free-form tree.

### Closed strong-typed enums

`StepKind` and `SpanKind` are closed enums; each variant carries its own typed input/output/provenance fields. There is no superset struct with `Option` fields, no `serde_json::Value` payload as a backdoor. New kinds are added by extending the enum, never by string tagging. (`SpanKind::ToolCall.params` and `output` use `serde_json::Value` because tool schemas are dynamic — that is the boundary, and it is the only one.)

```rust
pub enum StepKind {
    LlmIteration,
    Compression,
    MemoryRecall,
    MemoryWrite,
    SkillSelection,
    Subagent { child_session_id: SessionId },
}

pub enum SpanKind {
    LlmCall {
        begin: LlmCallBegin,            // model_id, provider, provider_config_hash, input_messages, temperature
        result: Option<LlmCallResult>,  // output_content, thinking, tool_calls, *_tokens (None while Pending)
    },
    ToolCall {
        begin: ToolCallBegin,           // tool_name, tool_artifact_hash, triggered_by, params
        result: Option<ToolCallResult>, // output, success (None while Pending)
    },
    SubagentStub { child_session_id },
}
```

### Step / Span cardinality rules

- One agent-loop iteration produces **exactly one** `Step` of kind `LlmIteration` containing 1 LLM `Span` + 0..N tool `Span`s. A pure-response iteration (no tool calls) still opens a Step containing one `LlmCall` span.
- Parallel tool calls are **sibling spans** under the same Step with the same `parallel_group: GroupId`. Their time windows may overlap.
- LLM ↔ tool pairing is by `ToolCallOrigin { llm_span_id, tool_use_id }`, not by tree structure. Tool spans are direct children of the Step.
- `Compression`, `MemoryRecall`, `MemoryWrite`, `SkillSelection` are first-class Step kinds, not events on an LLM step.
- `Subagent` is a Step kind. Its inner span is a `SubagentStub` that records nothing but the parent's wait window — the actual execution happens in `child_session_id`.

### Provenance lives on Span variants, not on Step

Each `SpanKind` variant carries the provenance fields that apply to it (`model_id` and `provider` on `LlmCall`, `tool_artifact_hash` on `ToolCall`). Step is a pure container with no provenance. Soul-version drift between session bind time and job effective time is recorded on `Job.provenance_drift` (see `job.md`).

### SpanEvent is sanitize / approval audit, not control flow

```rust
pub enum SpanEventKind {
    SanitizeHit { hits_count, kinds: Vec<SecretKind>, placeholder_ids: Vec<PlaceholderId> },
    Approval { decision: ApprovalDecision, resource: ResourceAccess },
}
```

- `SanitizeHit` is emitted **only when sanitize actually modified content**. Misses are not recorded — the trace records what happened, not what ran.
- `Approval` records **every** decision (`Approve`, `ApproveAlways`, `Deny`). The audit trail of "what did the user approve and when" is complete.

### Sanitization constraints

- Record only sanitized payloads — secrets appear only as placeholders
- `placeholder_ids` are kept in `SanitizeHit` so replay can resolve them via `SecretVault`
- Apply uniform sanitization to every `SpanKind` result variant — error paths included

### Single-table persistence

Step and Span lifecycle writes go to the canonical tables (`steps`, `spans`, `span_events`). Each row stores the entity as a single JSON `data` blob; queryable fields (`job_id`, `step_id`, `started_at`, `ended_at`) surface as `GENERATED ALWAYS AS (json_extract(...)) VIRTUAL` columns that SQLite keeps in lockstep with `data` automatically. There is no two-side write contract — adding a new field is a serde change in `aura-trace`, no schema migration. New indexed lookups need a new generated column; that is the only schema change vector.

The earlier two-layer WAL (`trace_events` table mirroring every begin/end) was removed once it became clear no reader consumed it: recovery scans `spans` directly, and there is no replay / OTel-export path yet that would benefit from the append-only log. If one lands later, the WAL can come back together with its consumer.

### Async writes with LLM/tool fences

Writes are asynchronous, with **synchronous fences** before any LLM or tool call: previous span's `end` and current span's `begin` must be durable before the request goes out. Other writes happen on a background writer task — the agent actor never blocks on persistence except at fences.

### Restart recovery

Not implemented yet. The schema indexes the half-open lookup (`spans.ended_at IS NULL AND deleted_at IS NULL`), and `CancelReason::SystemCrash` is reserved for it, but the scan + rewrite is not wired. After a crash, half-open spans stay half-open until an operator cancels the parent job via the admin API.

### Fork view-layer union

When listing jobs / steps / spans for a session whose `Lineage` is `UserFork { fork_at_job_id, ... }`, the read path UNIONs source-session rows up to the fork point with the new session's own rows, ordered by `created_at`. API responses tag inherited rows with `is_inherited: true` so UIs can render lineage without rewriting IDs. IDs themselves are unchanged — every backend lookup still works directly.

## Constraints

- Depends on `aura-job` (for `JobId`, `CancelReason`, `DriftRecord`) and `aura-model` (for `SessionId`, `ChatMessage`, `ContentBlock`, `SecretKind`, etc.). No dependency on `aura-storage`.
- IDs use ULID newtypes (`StepId`, `SpanId`); `SpanEvent` uses a `(span_id, seq)` compound key
- Storage uses columnar schema: `steps` / `spans` / `span_events` (one row per entity); the `Job > Step > Span` parent chain is encoded by foreign keys, not by embedded child lists
- All deletes are hard `DELETE FROM` — no `deleted_at` tombstone column (see [storage.md](./storage.md#hard-delete))
- `SpanRecorder` holds locks only for short critical sections, never across `await`
- `test_support::MemoryTraceStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `aura-trace = { workspace = true, features = ["test-support"] }`.

## Collaboration

| Module    | Role                                                                                                                         |
| --------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `job`     | Job manages state, Trace manages content; linked via `JobId`; `partial_artifacts: Vec<SpanId>` references trace spans         |
| `agent`   | Constructs and shares one `SpanRecorder` per session; uses `JobLifecycle` and `SpanRecorder` together as sibling facades       |
| `store`   | Owns the `TraceStore` trait + its `StepRow` / `SpanRow` / `SpanEventRow` DTOs and `StorageError`; this crate converts rich types ↔ rows |
| `storage` | Provides the libsql implementation of `TraceStore` (from `aura-store`), shuttling rows; depends on `aura-trace` only as a dev-dependency |
| `model`   | Provides `SessionId`, `ChatMessage`, `ContentBlock`, `SecretKind`, `PlaceholderId`, `ApprovalDecision`, `ResourceAccess`       |
