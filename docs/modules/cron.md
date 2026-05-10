# cron - Cron Jobs and Scheduler

## Overview

The `cron` crate owns scheduled recurring work end-to-end: domain types (`CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `ExecutionStatus`, `CronError`), the `CronScheduler` (`scheduler.rs`) that ticks them, the `Shutdown` trait (`shutdown.rs`) used to bound the scheduler's tick loop, and the LLM-invocable cron tools exposed by `agent_tools` (`tools.rs`). It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate) for recurring jobs and an absolute UTC instant for one-shot jobs. `aura-agent` re-exports `CronScheduler` and `CronTriggerEvent` for assembly-layer consumers.

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

### LLM-invocable cron tools

`agent_tools` (`crates/cron/src/tools.rs`) returns `CronCreateTool`, `CronDeleteTool`, and `CronListTool` `Tool` implementations. They live here (rather than in `aura-tools`) because they each hold `Arc<CronScheduler>`, and `aura-tools` cannot pull in `aura-cron` without creating a circular dependency.

### Storage decoupling

The `CronStore` trait in `storage` operates on opaque row types (`CronJobRow`, `CronExecutionRow`) with string fields — it does not depend on this crate. The `data` column holds a JSON blob that only `aura-cron` knows how to serialize/deserialize.

## Constraints

- No dependency on `agent`
- Depends on: `aura-model`, `aura-storage`, `aura-tools`, `chrono`, `chrono-tz`, `cron`, `tokio`, `parking_lot`, `serde`, `serde_json`, `uuid`, `async-trait`, `thiserror`, `anyhow`, `tracing`

## Collaboration

| Module | Role |
|--------|------|
| `storage` | `CronStore` trait persists opaque row types (`CronJobRow`, `CronExecutionRow`); `CronScheduler` consumes it |
| `tools` | `CronCreateTool` / `CronDeleteTool` / `CronListTool` implement `aura_tools::Tool` so the agent registry can dispatch them |
| `agent` | Re-exports `CronScheduler` / `CronTriggerEvent`; `Router` consumes the event stream, resolves sessions, and routes `AgentMessage::CronTrigger` to actors |
| `job` | `OperationKind::CronExecution` tracks cron-triggered operations |
