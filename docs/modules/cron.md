# cron - Cron Jobs and Scheduler

## Overview

The `cron` crate owns scheduled recurring work end-to-end: the `CronScheduler` (`scheduler.rs`) that ticks against the store, the `Shutdown` trait (`shutdown.rs`) used to bound the scheduler's tick loop, and `CronError`. The cron data types (`CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `ExecutionStatus`) live in `aura-model` (re-exported here for back-compat); the `CronStore` persistence trait lives in the `aura-store` ports crate. It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate) for recurring jobs and an absolute UTC instant for one-shot jobs. The libsql implementation of `CronStore` lives in `aura-storage`; the LLM-invocable cron tools (`CronCreate` / `CronDelete` / `CronList`) live in `aura-cron::tools` (the crate depends on `aura-tools` for the `Tool` trait). `aura-agent` re-exports `CronScheduler` and `CronTriggerEvent` for assembly-layer consumers.

CronJobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration. Each fire mints a brand-new session in the agent layer — one trigger = one session — so the run sees a clean transcript and fresh `SessionState`. A `CronJob` also records its `origin_session_id` — the session that created it — purely for traceability; trigger-time session creation is unaffected.

## Design Decisions

### Bind to user_id + channel, not session_id

Sessions are ephemeral (30-min default timeout). A cron job is a long-lived intent that must outlive any single session. Binding to `user_id + channel` provides a stable identity; the Router mints a fresh session per fire (UUID id, `TriggerSource::Cron { cron_job_id }` stamped at creation) and runs a one-shot actor that exits after `CronTrigger` + `Shutdown`. Continuity across fires lives in long-term memory, not in a shared mutable transcript — reusing one session would replay every prior fire's messages and `SessionState` into the next run.

### Pre-computed next_trigger_at

Each `CronJob` stores `next_trigger_at` — the next time it should fire. This allows the `CronScheduler` to query `WHERE next_trigger_at <= now` instead of parsing every cron expression on each tick. After each trigger, `next_trigger_at` is recomputed and persisted.

### One-shot lifecycle

Jobs whose `schedule` is `CronSchedule::At { time }` (i.e. `CronSchedule::is_one_shot()` returns true) transition to `CronStatus::Executed` after firing — the row is preserved (not deleted), so the web UI and history queries can still see "this fired and is done". `next_trigger_at` is cleared and `last_triggered_at` is stamped at the same time. The `list_due` query filter (`status = 'enabled'`) keeps `Executed` jobs from being re-fired by the tick loop. A `CronExecution` record is persisted alongside the status update; explicit `delete_job` is still available for callers that want to remove the row.

### Cron expressions are timezone-aware

Each `CronJob` carries an IANA `timezone` field (e.g. `"Asia/Shanghai"`, `"UTC"`). Cron expressions are evaluated **in that timezone**: `0 9 * * *` with `timezone = "Asia/Shanghai"` fires at 09:00 Shanghai time daily, not 09:00 UTC. The scheduler uses `chrono-tz` to convert the current UTC instant into the target zone, asks the `cron` crate for the next match in that zone, and converts the result back to `DateTime<Utc>` for persistence and the storage index. `At { time }` carries an absolute UTC instant and ignores `timezone`. Old rows persisted before this field existed deserialize with `"UTC"` — preserving their original behavior. The web admin auto-detects the browser's IANA zone via `Intl.DateTimeFormat()`, so users never need to pre-convert times.

### Schedule as a typed enum

The `schedule` field is `CronSchedule`, a tagged enum with two variants: `Cron { expr: String }` for recurring jobs and `At { time: DateTime<Utc> }` for one-shot jobs. The variant alone determines recurrence — there is no separate "run mode". Cron-expression parsing and validation happen at creation time in `CronScheduler`, not at the type level.

### Trigger payload: prompt only

`CronJob` carries `prompt: String` directly — every fire feeds `prompt` through the full agent loop and the LLM decides what tools (if any) to invoke. `CronExecution` records the same `prompt` as an immutable snapshot of what was actually executed at fire time.

### Fire-time framing: a fire is a task, not a user message

A fire is delivered to the model as a *user* turn, so a bare prompt is ambiguous: a job created to "say 你好 in a minute" stores the prompt `你好`, and at fire time the model reads `你好` as the user greeting it and greets back instead of performing the send. Two layers keep the intent unambiguous:

- **At creation**, the `CronCreate` tool (`aura-cron::tools`) steers the model to write `prompt` as a self-contained, imperative *task instruction* ("Send the user a greeting: 你好") rather than the literal phrase.
- **At fire time**, the agent layer wraps `prompt` via `aura_agent::cron_prompt::frame_cron_prompt` before it reaches the LLM. The framing states that this is a scheduled fire (not a live user message), that the prompt is an instruction to carry out now and report back, and that the `[cron:<job_id>]` routing tag is diagnostic-only and must never surface in the reply. `aura_agent::cron_prompt::original_cron_prompt` reverses the framing for operator previews (the admin chat panel) and stays backward-compatible with legacy `[cron:<id>] <prompt>` rows.

### LLM-invocable cron tools live in aura-cron

`tools::agent_tools` returns `CronCreateTool`, `CronDeleteTool`, and `CronListTool` `Tool` implementations (each holding an `Arc<CronScheduler>`). They live in `aura-cron::tools` — the same pattern as `aura-skills::tools` — so the cron domain owns its own LLM surface. This is only possible because `CronStore` moved to the `aura-store` ports crate: the old `aura-storage → aura-cron` edge is gone, so `aura-cron` taking a dependency on `aura-tools` (for the `Tool` trait) no longer closes the cycle `aura-cron → aura-tools → aura-storage → aura-cron`. `src/runtime.rs` registers them into the `ToolRegistry` after the scheduler is constructed.

### Storage decoupling

The `CronStore` trait lives in the `aura-store` ports crate (its libsql impl in `aura-storage`) and operates on the domain types directly — `CronJob` / `CronExecution` / `ExecutionStatus` rather than opaque row shapes. The libsql implementation in `aura-storage::libsql::cron` handles JSON serialization of the `data` column internally; the trait surface no longer leaks the row shape outside the backend. This is a deliberate change from the prior opaque-row design — the row-vs-domain dance has moved out of `aura-cron::scheduler` and into the libsql adapter where it belongs.

## Constraints

- No dependency on `agent` or `storage`. Depends on `aura-tools` (for the `Tool` trait the cron tools implement) and `aura-store` (the `CronStore` contract), so `aura-cron` is no longer a leaf — it mirrors `aura-skills`, which also carries its own `tools` module. No cycle: nothing in `aura-tools`'s dependency graph reaches back to `aura-cron`.
- Depends on: `aura-model`, `aura-store`, `aura-tools`, `chrono`, `chrono-tz`, `cron`, `tokio`, `parking_lot`, `serde`, `serde_json`, `uuid`, `async-trait`, `thiserror`, `anyhow`, `tracing`

## Collaboration

| Module | Role |
|--------|------|
| `storage` | Implements `CronStore` against libsql; depends on `aura-cron` for the trait |
| `tools`   | `aura-cron::tools` implements the `Tool` trait (`CronCreate` / `CronDelete` / `CronList`), bridging `Arc<CronScheduler>` to the registry; `src/runtime.rs` registers them |
| `agent`   | Re-exports `CronScheduler` / `CronTriggerEvent`; `Router` consumes the event stream, resolves sessions, and routes `AgentMessage::CronTrigger` to actors |
| `job`     | `OperationKind::CronExecution` tracks cron-triggered operations |
