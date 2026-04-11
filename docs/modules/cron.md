# cron - Cron Job Domain Types

## Overview

The `cron` crate defines domain types for scheduled recurring work: `CronJob`, `CronExecution`, `CronStatus`, `CronRunMode`, and `CronError`. It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate).

CronJobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration. Session resolution happens dynamically at trigger time in the agent layer.

## Design Decisions

### Bind to user_id + channel, not session_id

Sessions are ephemeral (30-min default timeout). A cron job is a long-lived intent that must outlive any single session. Binding to `user_id + channel` provides a stable identity; the Router resolves or creates a session at trigger time using a deterministic session ID (`cron-{user_id}-{channel}`).

### Pre-computed next_trigger_at

Each `CronJob` stores `next_trigger_at` — the next time it should fire. This allows the `CronScheduler` to query `WHERE next_trigger_at <= now` instead of parsing every cron expression on each tick. After each trigger, `next_trigger_at` is recomputed and persisted.

### One-shot eviction

`CronRunMode::OneShot` jobs are automatically deleted after firing. A `CronExecution` record is persisted before the job is deleted, preserving audit history. Execution records survive job deletion.

### Schedule stored as String

The `schedule` field stores the raw cron expression as a `String`. Parsing and validation happen at creation time in `CronScheduler` (in the `agent` crate), not at the type level. This keeps the domain type simple and serialization-friendly.

### Domain crate, not business logic

Like `aura-job` and `aura-session`, this crate only defines types and errors. The scheduler/manager business logic lives in `aura-agent::cron::CronScheduler`.

### Storage decoupling

The `CronStore` trait in `storage` operates on opaque row types (`CronJobRow`, `CronExecutionRow`) with string fields — it does not depend on this crate. The `data` column holds a JSON blob that only `agent::cron` knows how to serialize/deserialize. The agent layer bridges between domain types and storage rows via conversion functions.

## Constraints

- No dependencies on `agent`, `storage`, or other business crates
- Depends only on: `aura-session` (for `ChannelType`), `chrono`, `serde`, `thiserror`, `anyhow`
- Does not schedule or execute anything

## Collaboration

| Module | Role |
|--------|------|
| `storage` | `CronStore` trait persists opaque row types (`CronJobRow`, `CronExecutionRow`) |
| `agent` | `CronScheduler` manages lifecycle, converts between domain and row types, runs background tick loop, fires triggers |
| `agent::router` | Consumes `CronTriggerEvent`, resolves sessions, routes `AgentMessage::CronTrigger` to actors |
| `job` | `OperationKind::CronExecution` tracks cron-triggered operations |
