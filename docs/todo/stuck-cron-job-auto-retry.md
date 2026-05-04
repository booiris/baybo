# Stuck Cron Job Auto-Retry

## Problem

If startup recovery is added back, it will move `InProgress` Jobs to `Stuck` on restart. For cron-triggered Jobs, the `CronScheduler` has already advanced `next_trigger_at`, so it won't re-trigger the interrupted execution. The Stuck Job sits there with no automatic retry mechanism.

This issue is gated on recovery being wired again — see `docs/modules/job.md` "Restart recovery" (currently unimplemented).

## Proposed Direction

Once recovery exists, after the restart sweep scan for Stuck Jobs where `Job.kind == JobKind::Cron`. For each, either:
- Automatically re-dispatch via the actor (re-send `AgentMessage::CronTrigger`)
- Or move to `Failed` with a reason and let the user decide

Needs discussion on retry policy: immediate retry? backoff? max attempts?

## Related

- `agent::job::JobLifecycle`
- `agent::cron::CronScheduler`
- `agent::actor::AgentMessage::CronTrigger`
