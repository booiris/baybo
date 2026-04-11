# [RESOLVED] CronJob Lifecycle Mismatch: Bind to User, Not Session

> **Resolved**: Implemented in `aura-cron` crate and `agent::cron::CronScheduler`.
> CronJob is now bound to `user_id + channel`. Session is resolved dynamically
> at trigger time via `Router::handle_cron_trigger()` using stable session ID
> `cron-{user_id}-{channel}`. Persistence via `CronStore` / `LibsqlCronStore`.

## Problem

`CronJob` is currently bound to `session_id`, but sessions are ephemeral (30-min default timeout). When a session expires and the user starts a new conversation, cron jobs tied to the old session silently fail because `supervisor.route(old_session_id, ...)` finds no actor.

```
Timeline:
  Session A (30min timeout)
    User: "Push news every morning"
    CronJob { session_id: "A", prompt: "push news" }

  Session A expires, Actor destroyed

  Session B (user returns, new conversation)
    CronJob still points to session_id: "A"
    trigger() -> supervisor.route("A", ...) -> no actor found -> warn! silent failure
```

**Root cause:** Long-lived intent (scheduled task) bound to a short-lived resource (session).

## Proposed Solution

### 1. Bind CronJob to `user_id + channel` instead of `session_id`

```rust
// Before
pub struct CronJob {
    pub id: String,
    pub session_id: String,        // short-lived
    pub schedule: CronSchedule,
    pub prompt: String,
    pub enabled: bool,
}

// After
pub struct CronJob {
    pub id: String,
    pub user_id: String,           // stable identity
    pub channel: ChannelType,      // which channel to deliver to
    pub schedule: CronSchedule,
    pub prompt: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}
```

### 2. Resolve session dynamically at trigger time

```rust
// Before: route to a fixed session
supervisor.route(&job.session_id, AgentMessage::CronTrigger { ... })

// After: find or create session for the user
pub async fn trigger(&self, job_id: &str, supervisor: &AgentSupervisor) -> bool {
    let job = self.jobs.get(job_id)?;
    let session_id = supervisor
        .resolve_or_create_session(&job.user_id, &job.channel)
        .await;
    supervisor.route(&session_id, AgentMessage::CronTrigger { ... }).await
}
```

### 3. Alternatives considered

| Approach | Issue |
|----------|-------|
| Bind to `session_id` (current) | Session timeout orphans the cron job |
| Migrate cron jobs on session cleanup | Couples session lifecycle with cron; complex |
| Create a new session per trigger | Loses conversational context; each push is "stranger" |
| **Bind to `user_id + channel` (proposed)** | Stable identity; dynamic session resolution; no coupling |

## Required Changes

1. **Persist CronJob** - Currently `CronScheduler` is an in-memory `HashMap`; process restart loses everything. Add a `CronStore` trait or extend existing storage to persist cron jobs.

2. **Add `resolve_or_create_session` to AgentSupervisor** - Look up the user's active session on the given channel; if none exists or the previous one expired, create a new one via `SessionManager`.

3. **Decouple CronJob lifecycle from Session lifecycle** - Session cleanup must not delete associated cron jobs. Cron jobs are managed via explicit user action or a separate management interface.

4. **Handle context isolation for cron-triggered messages** - If the cron trigger reuses an active session where the user is mid-conversation on a different topic, consider either:
   - Tagging cron-triggered messages so the frontend/channel can display them separately.
   - Using a dedicated "cron session" per user+channel for scheduled deliveries.
