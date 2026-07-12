# Interrupted Cron Job Auto-Retry

## Problem

The boot recovery sweep cancels interrupted `InProgress` Jobs as `Cancelled { SystemCrash }` on restart (`crates/agent/src/recovery.rs`). For cron-triggered Jobs, the `CronScheduler` has already advanced `next_trigger_at`, so it won't re-trigger the interrupted execution. The cancelled Job sits there with no automatic retry mechanism.

Boot recovery now exists — see `docs/modules/job.md` "Recovery" and `crates/agent/src/recovery.rs` (`recover_orphaned_traces_and_jobs`, wired at boot in `crates/baybo/src/runtime.rs`).

## Proposed Direction

After the boot recovery sweep, scan for Jobs cancelled with `SystemCrash` where `Job.origin == TriggerKind::Cron`. For each, either:
- Automatically re-dispatch via the actor (re-send `AgentMessage::CronTrigger`)
- Or move to `Failed` with a reason and let the user decide

Note that `CronScheduler::recover_pending` (`crates/cron/src/scheduler.rs`) already re-dispatches executions that crashed before dispatch (`ExecutionStatus::Pending`). The remaining gap is only executions interrupted *after* dispatch (`ExecutionStatus::Dispatched` — see `crates/model/src/cron.rs`).

Needs discussion on retry policy: immediate retry? backoff? max attempts?

## Related

- `baybo_job::JobLifecycle` (`crates/job/src/lifecycle.rs`)
- `baybo_cron::CronScheduler` (`crates/cron/src/scheduler.rs`)
- `agent::actor::AgentMessage::CronTrigger`
