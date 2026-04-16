# trace - Call Chain Tracing Types and Utilities

## Overview

The `trace` crate defines domain types for call-chain tracing (`SessionTrace`, `TraceNode`, `TraceSpan`, `SpanHandle`, `SpanInput`, `SpanResult`, `ExecutionProvenance`, `ForkRecord`, `TraceFilter`) and provides tree, fork, and snapshot utility functions.

Business logic (`TraceCollector` — span lifecycle management and persistence) lives in `agent::trace`. The `TraceStore` trait is defined in `storage::trace`.

Trace answers **"what exactly did this operation do"** by recording sanitized inputs, results, latency, and execution provenance. Its difference from `job` is: **Job manages state, Trace manages content.**

## Design Decisions

### Tree structure, not list

One LLM call may spawn multiple child operations (tool calls, memory operations), so Trace uses a tree. `active_leaf` points to the current active leaf; new spans are attached below it by default.

### begin/end lifecycle

`begin_span()` creates a node with kind, job_id, provenance, and input. After execution, `end_span()` fills in `ended_at` and result. Upper layers should use `ObservabilityRecorder` to create Job and Trace records together.

### Sanitization constraints

- Record only sanitized payloads — secrets appear only as placeholders
- `SpanResult::LlmResponse` records full `output_content`, `thinking` (reasoning), and `tool_calls`

### Provenance for replayability

If the system supports skill hot reload, tool replacement, Soul config updates, or provider config changes, input/output alone is insufficient — version source must be recorded. Otherwise historical replay becomes "rerun yesterday's conversation with today's code," which is not auditable.

Provenance fields: `skill_version`, `tool_artifact_hash`, `provider_config_hash`, `soul_version`.

### Snapshots and rollback

- Save a `ContextSnapshot` automatically every N spans
- Snapshots store only logical messages and blob references, never raw media
- If no snapshot exists at the target node, walk up the parent chain to find the nearest one
- Rollback: fork from target node, restore session messages and context state from snapshot

### Branch semantics

Branching creates a new branch below the target node without overwriting the original chain. Both branches are preserved for audit and comparison.

## Constraints

- Types crate with tree/fork/snapshot utilities — no collector logic
- Depends on `context` (for `ContextSnapshot`) and `job` (for `OperationKind`)
- `TraceCollector` (in `agent::trace`) should lock only for short critical sections, never across `await`
- Apply uniform sanitization to `SpanResult::Error` to prevent sensitive data leaking through exception paths
- `save_trace()` should save the whole tree in one transaction

## Collaboration

| Module | Role |
|--------|------|
| `context` | Provides `ContextSnapshot` for rollback restoration |
| `job` | Job manages state, Trace manages content; linked via `job_id` and `trace_span_id` |
| `agent` | `agent::trace::TraceCollector` owns span lifecycle; `ObservabilityRecorder` creates Job and Trace records together |
| `storage` | Defines `TraceStore` trait using trace types; provides libsql implementation |
