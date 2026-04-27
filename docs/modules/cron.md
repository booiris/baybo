# cron - Cron Job Domain Types

## Overview

The `cron` crate defines domain types for scheduled recurring work: `CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `TriggerAction`, `ExecutionStatus`, and `CronError`. It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate) for recurring jobs and an absolute UTC instant for one-shot jobs.

CronJobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration. The Router mints a fresh session per fire stamped with `SessionTrigger::Cron { cron_job_id, scheduled_fire_time }`; conversational continuity across fires lives in long-term memory (vector store) and the skill loader, not in a shared mutable transcript. A `CronJob` also records its `origin_session_id` — the session that created it — purely for traceability.

## Design Decisions

### Bind to user_id + channel, not session_id

Sessions are ephemeral (30-min default timeout). A cron job is a long-lived intent that must outlive any single session. Binding to `user_id + channel` provides a stable identity. The Router mints a brand-new session (UUID id) on every fire and tags its `SessionTrigger` with the cron job id and scheduled fire time. Each fire's actor is one-shot: spawned, sent the `CronTrigger` message, then sent `Shutdown`; the actor processes the trigger, exits, and the sender drops without registering with the supervisor — so per-fire actors don't accumulate.

### Pre-computed next_trigger_at

Each `CronJob` stores `next_trigger_at` — the next time it should fire. This allows the `CronScheduler` to query `WHERE next_trigger_at <= now` instead of parsing every cron expression on each tick. After each trigger, `next_trigger_at` is recomputed and persisted.

### One-shot eviction

Jobs whose `schedule` is `CronSchedule::At { time }` (i.e. `CronSchedule::is_one_shot()` returns true) are automatically deleted after firing. A `CronExecution` record is persisted before the job is deleted, preserving audit history. Execution records survive job deletion.

### Schedule as a typed enum

The `schedule` field is `CronSchedule`, a tagged enum with two variants: `Cron { expr: String }` for recurring jobs and `At { time: DateTime<Utc> }` for one-shot jobs. The variant alone determines recurrence — there is no separate "run mode". Cron-expression parsing and validation happen at creation time in `CronScheduler` (resident in this crate), not at the type level.

### Two trigger modes: `Prompt` vs `ToolCall`

`TriggerAction` is a tagged enum (`#[serde(tag = "kind")]`) with two variants:

- `Prompt { prompt }` — feeds `prompt` through the full agent loop every fire. Use for open-ended tasks ("summarize overnight news") where the LLM decides what tools to invoke.
- `ToolCall { tool_name, params, approved_resources }` — directly invokes a registered tool, bypassing the LLM. Use for deterministic recurring work (fetch a URL, run a backup script). `approved_resources` is captured at creation time so the execution does not need to prompt the user on every fire. If the direct call fails, the agent layer falls back to dispatching a diagnostic prompt through the LLM so the failure surfaces as a normal reply.

The same enum is stored on `CronExecution` as an immutable snapshot of what was actually executed at fire time.

### Domain crate, not business logic

This crate owns both the domain types and the `CronScheduler` business logic (background tick loop, trigger dispatch, restart re-dispatch). Storage adapter logic lives in `aura-storage`, and the LLM-facing `CronCreateTool` / `CronDeleteTool` / `CronListTool` (returned from `aura_cron::agent_tools`) are exported from this crate so they can hold `Arc<CronScheduler>` without creating a circular dep on `aura-tools`.

### Storage decoupling

The `CronStore` trait in `storage` operates on opaque row types (`CronJobRow`, `CronExecutionRow`) with string fields. The `data` column holds a JSON blob that this crate's `CronScheduler` knows how to serialize/deserialize via row-conversion helpers.

## Constraints

- No dependency on `aura-agent` (would create a cycle: agent already depends on cron). Depends on `aura-storage` for `CronStore` row types and `aura-tools` for the `Tool` trait used by `agent_tools`.
- Owns the scheduler tick loop, trigger dispatch, and the LLM-facing cron tools.

## Collaboration

| Module | Role |
|--------|------|
| `storage` | `CronStore` trait persists opaque row types (`CronJobRow`, `CronExecutionRow`) |
| `agent` | `Router::handle_cron_trigger` consumes `CronTriggerEvent`s and spawns one-shot per-fire actors |
| `agent::router` | Consumes `CronTriggerEvent`, resolves sessions, routes `AgentMessage::CronTrigger` to actors |
| `job` | `OperationKind::CronExecution` tracks cron-triggered operations |
