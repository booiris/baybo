# trace - Call Chain Tracing

## Overview

The `trace` crate records the tree of all key operations during session processing: user input handling, LLM calls, tool execution, skill execution, context compression, memory operations, and rollback/branching.

Trace answers **"what exactly did this operation do"** by recording sanitized inputs, results, latency, and execution provenance. Its difference from `job` is: **Job manages state, Trace manages content.**

## Design Decisions

### Tree structure, not list

One LLM call may spawn multiple child operations (tool calls, memory operations), so Trace uses a tree. `active_leaf` points to the current active leaf; new spans are attached below it by default.

### begin/end lifecycle

`begin_span()` creates a node with kind, job_id, provenance, and input. After execution, `end_span()` fills in `ended_at` and result. Upper layers should use `ObservabilityRecorder` to create Job and Trace records together.

### Sanitization constraints

- Record only sanitized payloads — secrets appear only as placeholders
- Outputs keep only previews or summaries
- `reasoning_redacted = true` means provider reasoning was not persisted
- `SpanResult::LLMResponse` uses `output_preview` instead of full output

### Provenance for replayability

If the system supports skill hot reload, WASM tool replacement, Soul config updates, or provider config changes, input/output alone is insufficient — version source must be recorded. Otherwise historical replay becomes "rerun yesterday's conversation with today's code," which is not auditable.

Provenance fields: `skill_version`, `tool_artifact_hash`, `provider_config_hash`, `soul_version`.

### Snapshots and rollback

- Save a `ContextSnapshot` automatically every N spans
- Snapshots store only logical messages and blob references, never raw media
- If no snapshot exists at the target node, walk up the parent chain to find the nearest one
- Rollback: fork from target node, restore session messages and context state from snapshot

### Branch semantics

Branching creates a new branch below the target node without overwriting the original chain. Both branches are preserved for audit and comparison.

## Constraints

- Depends on `core` and `context` (for `ContextSnapshot`)
- `TraceCollector` should lock only for short critical sections, never across `await`
- Apply uniform sanitization to `SpanResult::Error` to prevent sensitive data leaking through exception paths
- `save_trace()` should save the whole tree in one transaction

## Collaboration

| Module | Role |
|--------|------|
| `context` | Provides `ContextSnapshot` for rollback restoration |
| `job` | Job manages state, Trace manages content; linked via `job_id` and `trace_span_id` |
| `agent` | `ObservabilityRecorder` creates Job and Trace records together |
| `storage` | Provides `TraceStore` implementations |
