# Stuck Cron Job Auto-Retry

## Problem

When the application restarts, `JobManager::recover_interrupted()` moves `InProgress` Jobs to `Stuck`. For cron-triggered Jobs, the `CronScheduler` has already advanced `next_trigger_at`, so it won't re-trigger the interrupted execution. The Stuck Job sits there with no automatic retry mechanism.

## Proposed Direction

On startup (after `recover_interrupted()`), scan for Stuck Jobs where `OperationKind == CronExecution`. For each, either:
- Automatically re-dispatch via the actor (re-send `AgentMessage::CronTrigger`)
- Or move to `Failed` with a reason and let the user decide

Needs discussion on retry policy: immediate retry? backoff? max attempts?

## Related

- `agent::job::JobManager::recover_interrupted()`
- `agent::cron::CronScheduler`
- `agent::actor::AgentMessage::CronTrigger`
