# cron - Cron Jobs and Scheduler

## Overview

The `cron` crate owns scheduled recurring work end-to-end: the `CronScheduler` (`scheduler.rs`) that ticks against the store, the `Shutdown` trait (`shutdown.rs`) used to bound the scheduler's tick loop, and `CronError`. The cron data types (`CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `ExecutionStatus`, `ExecutionOutcome`, `PendingCronResult`) live in `baybo-model` (re-exported here for back-compat); the `CronStore` persistence trait lives in the `baybo-store` ports crate. It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate) for recurring jobs and an absolute UTC instant for one-shot jobs. The sqlite implementation of `CronStore` lives in `baybo-storage`; the LLM-invocable cron tools (`CronCreate` / `CronUpdate` / `CronDelete` / `CronPause` / `CronResume` / `CronList`) live in `baybo-cron::tools` (the crate depends on `baybo-tools` for the `Tool` trait). `baybo-agent` re-exports `CronScheduler` and `CronTriggerEvent` for assembly-layer consumers.

CronJobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration. Each fire mints a brand-new session in the agent layer — one trigger = one session — so the run sees a clean transcript and fresh `SessionState`. A `CronJob` also records its `origin_session_id`: the conversation it was created from, which for a one-shot is where the fire's result is reported back.

## Where a fire's result goes

The two schedule shapes have genuinely different notification stories, and the design leans into that rather than papering over it. Both are **deterministic**: no LLM inference sits in the delivery path, so the model can never digest a scheduled reminder into silence, and each fire costs exactly one inference.

### Recurring (`CronSchedule::Cron`) — the fire opens a conversation

Every fire's fresh session is a **first-class conversation**: `TriggerSource::Cron { conversation: true }`, titled `{job title} · {M/d}` (the fire's date in the job's own timezone), listed in the chat sidebar, replyable, pinnable, and pushable. Being replyable has two consequences for its actor, and both are load-bearing: it is **registered** with the supervisor (an unregistered actor would let a reply fork a second actor over the same transcript), and it is **not stopped** when the fire ends (it is reclaimed by the idle reaper, like every other conversation's). Stopping it immediately would be worst exactly when it matters most — the moment after a notification lands is when a user is most likely to reply, and a reply that raced the stop would be routed into a mailbox nobody is reading, reported as delivered, and dropped. The fire's reply dispatches out through the channel exactly as before — the conversation *is* the notification, and the user can follow up in it ("expand on the third item"), because the fire's framed prompt and its reply are both right there in the transcript.

The next fire still mints a **new** conversation; a fire never appends to a previous one (see the clean-transcript decision below).

A fire that produces no reply — it failed, or it ran and said nothing — would otherwise leave a conversation that is empty when opened and, having dispatched nothing, never announced itself at all (clients learn a new conversation exists from the activity pulse the gateway derives from *channel dispatch*). So those outcomes publish the **same framed notification row** a one-shot delivers to its origin: `⏰ Scheduled task "…" failed:` or `It ran, but produced no output.`

A `*/30 * * * *` job opens 48 conversations a day, so the fires are collapsed into one chat-list row — a **cron group** — rather than landing flat next to real conversations. The grouping is *derived*, not stored: every fire already carries `TriggerSource::Cron { cron_job_id }`, so clients group on it and the gateway supplies the label. No folder row is created; `sessions.folder_id` and the folder tree are **not** used. See [`docs/cron-groups.md`](../cron-groups.md), which also records why the obvious folder-row design was rejected — it stores the same fact twice and every mechanism that keeps the copies in sync has a way to leave behind a cron folder that no API can delete.

Grouping is not a mute: a noisy job still fires, still costs an inference, and still pushes. The group's badge can be cleared in bulk (`POST /v1/chat/sessions/read`), but a user who is tired of a job still deletes it.

### One-shot (`CronSchedule::At`) — the fire reports back to the conversation that scheduled it

A one-shot's session is a **private workspace**: it is not listed, cannot be attached to, and dispatches nothing (`CronDelivery::OriginSession`). Its actor is unregistered — nothing will ever route to it again. Its result is delivered into the conversation that created the job, where the user is actually looking:

1. **Waiter.** When the Router dispatches a one-shot fire it also spawns a waiter (`actor/router/cron.rs`), subscribed to the `JobLifecycle` bus **before** the trigger is sent — the fire's terminal event is published from inside its own turn, so a later subscription could miss it. The waiter also watches the fire actor's token, so an actor that dies before opening a job still produces a notification (reconciled against the store, then reported as a failure) rather than nothing. It runs **even when the trigger cannot be handed to the actor at all** (a closed mailbox during a shutdown race): the scheduler has already recorded that execution as dispatched, so nothing else would ever retry it — the Router trips the token instead, and the waiter reports the fire as failed.
2. **Ledger.** On the terminal edge the waiter stamps the outcome onto the `CronExecution` — `fire_session_id`, `outcome`, `reply_ordinal`, `completed_at` — **before** delivering, so a crash in the delivery window leaves a durable record instead of a lost result.
3. **Delivery.** The waiter routes `AgentMessage::CronResultReady` to the origin conversation's actor via `route_or_spawn`, hydrating it if the idle reaper had reclaimed it. The message sits in the `BackgroundJobFinished` mailbox tier: behind a queued user turn, ahead of a stop.
4. **Append (no inference).** At the origin's next turn boundary the actor dedups on `execution_id`, un-hides the conversation if the user had removed it from their list, reads the fire's reply row, appends it as a `MessageSource::CronNotification` assistant row framed with a scheduled-task header, dispatches it to the channel, and resolves the ledger (`notified_at`). An append that does not reach the store is a **failed** delivery, not a quiet one — the row would live only in the actor's memory and the push would have no durable row to preview — so the ledger is left unresolved and the re-drive retries it.

Timeliness costs nothing here: the only wait is an origin actor that happens to be mid-turn, and splicing a notification into a reply the user is watching stream would be worse than the few seconds it takes for that turn to end.

### Unread comes for free

Both paths land an ordinary assistant row in an ordinary session, so the existing read-state machinery covers them with no cron-specific work: the fresh recurring conversation has no `read_cursor`, so its reply counts as unread; the one-shot's notification lands past the origin's cursor and bumps that conversation's badge. Live signalling works too — `session_pulse` observes channel dispatch, so both the fire's reply and the origin append pulse `Frame::SessionActivity`. Two silences are by design: a blank reply is suppressed before dispatch, and a one-shot fire session in `OriginSession` mode dispatches nothing (its signal comes from the origin instead).

### Every outcome notifies — for both kinds of fire

A scheduled task that silently evaporates is the worst failure this feature can have, so **every** fire produces exactly one notification, whichever kind it is and however it ended. There is no blank-reply suppression (unlike `SubagentNotification`, where the model's silence is a legitimate "not worth interrupting" judgment):

| Fire outcome | One-shot → origin conversation | Recurring → its own conversation |
|---|---|---|
| Success | Header + the fire's reply, plus any media | The reply itself (the conversation *is* the report, so no header is added) |
| Blank (ran, said nothing) | Header + "It ran, but produced no output." | The same framed notification row |
| Failed (error / cancelled) | Failure header + the reason from the job row | The same framed notification row |

The non-reply outcomes go out as a real `CronNotification` **assistant row**, never as a `Notice`, and the difference is not cosmetic. A row survives a reload, is read back by the model on a follow-up turn, raises the conversation's unread badge, and rides its `CronNotification` job's `Completed { reply_ordinal }` edge to the user's phone. A notice does none of those — a failed scheduled task would surface as an unbadged, unpushed conversation the user has to spot for themselves.

Two behaviour changes fall out of this, both deliberate: a fire that produces nothing now says so (it used to be dropped on the floor), and a **failed recurring fire now pushes** (an earlier revision reported it as a notice, and therefore not at all on a phone).

## Design Decisions

### Framing lives in the persisted content, not in a wire envelope

The notification's header (`⏰ Scheduled task "{title}" ran:`) is built by `baybo_context::prompts::cron::frame_cron_notification` and baked into the row that is stored. The other framed rows (user interjections, recalled memory) apply their envelope wire-side, but those are `Role::User` rows the model reads as input. This one is `Role::Assistant` — the model reads it back as something *it already said*, so it must see the same bytes the user saw, or the two would disagree about what was reported. Baking it in also means a boot-time re-delivery reproduces the row exactly, and that the header (not a web-only chip) is what identifies the message on every surface: web, Telegram, the APNs preview, and the model's own context.

The header is **English**, like every other prompt in the tree — the model reads this row back as its own words, and a header fixed in some other language would put words in its mouth it never chose. The *body* is the fire's own reply, so a job whose prompt asks for a Chinese reminder still reports in Chinese, under an English header.

### At-least-once delivery, exactly-once transcript

The ledger fields on `CronExecution` (`completed_at` … `notified_at`) make delivery recoverable. `notified_at` records a **resolution**, not merely a success: it is stamped both when the result lands and when it is terminally dropped (no usable origin), so the boot re-drive's scan (`completed_at IS NOT NULL AND notified_at IS NULL`, both projected into indexed columns) converges instead of re-attempting a hopeless delivery on every boot.

At boot, `Router::run` re-drives that scan before any live traffic. The origin actor appends with the source-event key `cron-execution:<execution_id>`; a unique partial index on `(session_id, source_event_id)` claims the key in the same statement as the transcript row. A replay therefore returns the original ordinal without opening another notification job, appending another row, or dispatching another live message, then stamps `notified_at` so the scan converges.

The former session-row cache (`delivered_cron_executions`) and its append→cache crash window no longer exist. Transcript insertion is exactly-once for a source execution even after compaction because the unique key remains on the historical superseded row. The execution ledger stamp is still a separate write; its replay is safe because it now meets the database constraint rather than a bounded in-memory/session-state cache.

This is a transcript guarantee, not a distributed exactly-once claim. The
notification job and external channel/push dispatch are not in the same
database transaction as `session_messages`: a crash after the row lands can
leave the durable conversation/unread state intact without completing every
proactive transport. Replaying that execution deliberately converges on the
existing row instead of risking a duplicate push. A transactional outbox would
be required to close that transport-level gap.

Accepted gap: if the process dies after the fire's job reaches a terminal state but before the waiter stamps `completed_at`, the execution is not re-driven. It neighbours the interrupted-execution gap tracked in [`../todo/stuck-cron-job-auto-retry.md`](../todo/stuck-cron-job-auto-retry.md).

### Fallbacks: when there is no conversation to report to

The test for a delivery target is **"can the user open this and read it"**, not "was it started by cron". So a one-shot's delivery is **dropped** (and its ledger resolved) only when the origin genuinely cannot receive it: no `origin_session_id` recorded, the session no longer resolves, or the origin is a **one-shot fire's own workspace** — invisible and unopenable, so a notification there would reach nobody. A *recurring* fire's conversation is a legitimate target: it is listed and replyable, and a job really can be scheduled from inside one.

That last point also fixes where a chained job anchors. Cron fire sessions get the full tool registry, so `CronCreate` can run inside a fire — and a user replying in a recurring fire's conversation can ask for a reminder there. `origin_session` (`baybo-cron::tools`) therefore anchors on the **calling session** in every case except one: a job created inside a *one-shot* fire's workspace inherits that fire's own origin, collapsing the chain onto the real conversation. Anchoring on the workspace would deliver into a session nobody can open; inheriting for a *recurring* conversation would report the result into some other conversation entirely (or, for a job created via the admin API with no origin at all, drop it silently).

### Bind to user_id + channel, not session_id

Sessions are ephemeral (30-min default timeout). A cron job is a long-lived intent that must outlive any single session. Binding to `user_id + channel` provides a stable identity; the Router mints a fresh session per fire (a `cron-`-prefixed UUID id, `TriggerSource::Cron` stamped at creation) and runs a one-shot actor that exits after `CronTrigger` + `ActorStop`. Continuity across fires lives in long-term memory, not in a shared mutable transcript — reusing one session would replay every prior fire's messages and `SessionState` into the next run. (This is why a recurring job opens a *new* conversation per fire rather than appending to one long-lived thread.)

### The visibility marker is on the trigger, not the `hidden` flag

`TriggerSource::Cron` carries `conversation: bool` (and `origin_session_id`), both `#[serde(default)]`. The Router sets `conversation: true` only for recurring fires. Effects: new recurring fires are listed; one-shot workspaces are not; and **every session persisted before this field existed deserializes `false`** — so a backlog of historical fires stays out of the sidebar with no migration. `include_cron=true` remains the operator escape hatch that shows all of them.

Rejected: marking one-shot fire sessions `hidden = true` at birth. `hidden` is the user's own "remove from my list" affordance; conflating the two would pollute un-hide semantics and admin views, and it would do nothing for legacy rows.

### Pre-computed next_trigger_at

Each `CronJob` stores `next_trigger_at` — the next time it should fire. This allows the `CronScheduler` to query `WHERE next_trigger_at <= now` instead of parsing every cron expression on each tick. After each trigger, `next_trigger_at` is recomputed and persisted.

### One-shot lifecycle

Jobs whose `schedule` is `CronSchedule::At { time }` transition to `CronStatus::Executed` after firing — the row is preserved, so the web UI and history queries can still see "this fired and is done", and so an edit can give the job a new moment without minting a new one (see [Editing a job in place](#editing-a-job-in-place)). `next_trigger_at` is cleared and `last_triggered_at` is stamped at the same time. The `list_due` query filter (`status = 'enabled'`) keeps `Executed` jobs from being re-fired by the tick loop. A `CronExecution` record is persisted alongside the status update.

### Pause and resume: `status` is the firing switch

`CronStatus` decides whether a job fires, and it is the only thing pause/resume touch. `CronScheduler::disable_job` flips the job to `Disabled` and clears `next_trigger_at`: it keeps its place in every list, and `list_due`'s `status = 'enabled'` filter takes it out of the tick loop. `enable_job` flips it back to `Enabled` and recomputes `next_trigger_at` **from now**.

Recomputing from now is the whole point: the slots that came and went while the job was paused are **not** made up. A daily job paused for a week and resumed today fires once tomorrow — not seven times the instant it comes back. A one-shot whose instant has already passed has nothing left to fire at all, so `enable_job` refuses it with `CronError::InvalidSchedule` rather than enabling a job whose `next_trigger_at` would be `None` forever; the way to give it a fire time again is to edit it, which keeps the job's id and its history.

Three surfaces drive the pair, all over those two scheduler methods: the `CronPause` / `CronResume` tools, `POST /v1/cron/{id}/pause` and `/resume` on the admin API (204; a resume that has no future fire time is the 400), and the pause/resume button the web cron page renders on each row. `CronResume` hands the recomputed `next_trigger_at` back to the model in the job's own timezone, so its reply can say when the job next fires. A fired one-shot (`Executed`) offers neither control: there is no slot left to pause, and its instant has gone, so there is nothing for `enable_job` to compute. Giving it a *new* instant is an edit, not a resume — see [Editing a job in place](#editing-a-job-in-place).

Both reach a job by id, and `get` resolves deleted jobs — so both check `is_deleted` and report a job in the bin as `NotFound` (404), exactly as `trigger_now` does. Resuming a binned job could not put it back on the schedule anyway (`list_due` never sees it), so a success would tell the user — or, through `CronResume`'s reply, the model — that a job is firing again when it never can; and pausing one would overwrite the status `restore_job` promises to bring it back with. `restore_job` is the only way out of the bin.

### Deletion is a recycle bin

`CronJob::deleted_at` (`Option<DateTime<Utc>>`; `None` = live) is the tombstone. `CronStore::delete` stamps it, `CronStore::restore` clears it, and the row itself is never removed — no store method issues a `DELETE FROM cron_jobs`, and no caller asks for one.

The row has to survive because everything the job produced outlives it: its `cron_executions` rows, the conversations its recurring fires opened, and the notifications its one-shots appended into other conversations. The job row is the only thing tying those back to their origin — drop it and a fire's conversation is provenance-less and its execution rows are orphans pointing at a `job_id` that resolves to nothing. `CronStore::get` therefore resolves a deleted job by id **on purpose**: that lookup is what keeps "this conversation came from the scheduled task *Standup reminder*" answerable after the user has thrown the task away.

Deletion is **orthogonal to `status`**. A deleted one-shot that already fired keeps `Executed`; a deleted enabled job keeps `Enabled`. There is no `CronStatus::Deleted` variant: status and deletion are two independent axes, and folding one into the other would make "bring it back exactly as it was" unrepresentable.

**The listing invariant.** Every query that can feed the tick loop or a user-facing list filters `deleted_at IS NULL` **in SQL** — `list_due`, `list_enabled`, `list_by_user`, `list_all`. Never in Rust: a filter applied after the rows come back is one forgotten `.filter()` away from a deleted job firing, and a scheduled task that keeps firing after the user deleted it is the worst outcome this feature has. `list_deleted` is the sole inversion (`deleted_at IS NOT NULL`, most recently deleted first) and it serves the recycle bin alone. The partial index `idx_cron_jobs_live_due` on `(status, next_trigger_at) WHERE deleted_at IS NULL` keeps the tick query indexed.

The one path that fires a job without going through a listing is `CronScheduler::trigger_now`, which reaches it by id — and `get` resolves deleted jobs by design. So `trigger_now` checks `is_deleted` itself and reports a deleted job as `NotFound`: a job in the bin does not fire on demand any more than it fires on schedule.

**Restore recomputes the schedule *before* it un-hides the row.** A job deleted on Monday still carries Monday's `next_trigger_at`; restore it on Thursday and that instant is three days overdue. Publishing the row with that stale slot would fire it on the very next tick — the exact catch-up burst pause/resume refuses to do. So `CronScheduler::restore_job` writes the recomputed schedule **while the row is still in the bin**, then clears `deleted_at` in a second write. The intermediate state — still deleted, already carrying a fresh schedule — is invisible to the tick loop and safe to crash in (the restore is simply retried). What it publishes:

| Status while deleted | Restored as |
|---|---|
| `Enabled`, recurring | `Enabled`, `next_trigger_at` = the next slot **after now**; the slots missed in the bin are not back-filled |
| `Enabled`, one-shot whose instant is still ahead | `Enabled`, firing at that instant |
| `Enabled`, one-shot whose instant passed while deleted | `Disabled`, no trigger — there is nothing left to fire |
| `Disabled` | `Disabled`, no trigger — a paused job comes back paused |
| `Executed` | `Executed` — a one-shot that already ran stays run |

Restoring a job that is not in the bin is a no-op rather than an error, so a double-click on the restore button cannot reschedule a live job.

**A result already computed is still owed; a fire that never ran is not.** Both boot scans walk *executions*, not jobs, and the delete cuts between them:

- **The delivery re-drive** (`list_executions_awaiting_delivery`: `completed_at IS NOT NULL AND notified_at IS NULL`) replays a result the fire already produced. It runs for a deleted job on purpose — swallowing a result the user is waiting on because the task was tidied away in the meantime would be exactly the silent evaporation the delivery ledger exists to prevent. The notification names the job by its snapshotted title, and `get_job` still resolves the row behind it.
- **The pending-execution recovery** (`recover_pending`: `status = Pending`) is not a delivery. A `Pending` row has never been dispatched — nothing has run and there is no result to owe — so re-dispatching it *fires the job*. It therefore checks job liveness the same way `list_due` does, and a job in the bin does not fire: a deleted task firing a fresh conversation on the next restart is precisely what the bin promises will not happen. The orphaned row is retired rather than left `Pending`, so a later restore cannot resurrect the stale fire either.

### Editing a job in place

A job is edited, not replaced. `CronScheduler::update_job(job_id, CronJobPatch)` writes a **partial patch** — `title`, `prompt`, `schedule`, `timezone`, each optional, each left untouched when the patch does not carry it. A patch that sets nothing is `CronError::EmptyUpdate`: there is nothing to write, and reporting success would tell the user their job changed when nothing about it did. It is always a caller bug, so it surfaces as a 400 / `InvalidParams` rather than a no-op.

The job keeps its **id**, and that is the whole point. Its `cron_executions` rows and the conversations its recurring fires opened are bound to that id; a job that is deleted and re-created under a new one leaves them pointing at a row that is now a tombstone in the bin, and the user's "move the reminder to 8am" has silently cut the task's history in half. Editing is the path every surface takes to change a job, and `CronUpdate`'s description says so outright so the model does not reach for delete + create.

`status`, `next_trigger_at` and `last_triggered_at` are **not** in the patch. They are the scheduler's, derived from the schedule and moved by pause/resume — a caller says what the job *is*, never where it sits in the firing schedule.

**A blank prompt is refused, on creation and on edit alike** (`CronError::BlankPrompt`, whitespace included). A job with no instruction is not a job that does nothing: it keeps its schedule and fires an empty prompt on every slot. `validate_prompt` is the single gate both entry points go through, which is what closes the API client's path to it — the LLM tools have their own reason to filter a blank field out (a model that sends `""` for "leave this alone" would otherwise hollow out a live job), and the web form will not submit one, but neither of those guards the raw REST surface.

**Rescheduling recomputes from now, and never back-fills.** Changing `schedule` **or** `timezone` re-arms the job at the first slot strictly after now (a timezone change moves the job on its own: the same expression names a different instant in a different zone). The slots that passed under the old schedule are not made up — the same rule pause/resume and restore already follow, for the same reason: an edit that fires seven times the instant it lands is a worse answer than one that fires tomorrow. A new `At` whose instant has already gone is refused with `CronError::InvalidSchedule`, exactly as `enable_job` refuses one, because arming it would leave a job with no fire time forever.

That refusal comes from `arm_schedule` — **the single gate every entry point that arms a job goes through**, `create_job`, `update_job` and `enable_job` alike, so the three cannot drift on what they accept: a cron expression the parser rejects, a timezone that will not resolve, or a one-shot whose moment has passed.

**Every in-place change is a conditional write.** An edit, a pause and a resume all read a job, change a few fields, and write the **whole** record back — so any of them can be raced by a fire's write-back or by another of them. They therefore share one path, `CronScheduler::edit_in_place`, whose write is `CronStore::save_if_unchanged`: it lands only while the stored row is still the snapshot that was read (same `status`, same `next_trigger_at`, same `updated_at`, still live), and otherwise the change is re-applied to the row as it now stands. An unconditional `save` here is the bug it looks like it isn't: a pause carrying a snapshot read a moment before an edit landed would put the old prompt back — the user is told the edit saved, and it is gone by the next refetch — and would forget a fire that landed in the same window. `updated_at` is what makes the check a real version check: an edit that touches only authored fields moves neither projected column, so two concurrent edits would both match on status and slot and the second would silently overwrite the first.

A fire's write-back is the one write that is *not* a whole-record write, and that is why it is `record_fire` instead: it stamps its four fields onto the row as stored, so an edit that lands mid-fire keeps the prompt the user typed.

**What an edit does to a job's firing state:**

| Status when edited | A patch that leaves the schedule and timezone alone | A patch that moves `schedule` or `timezone` |
|---|---|---|
| `Enabled` | The patched fields are written; `next_trigger_at` does not move | `next_trigger_at` = the first slot after now |
| `Disabled` | The patched fields are written; still paused, still no trigger | Still paused, still **no** trigger — the new schedule is validated, but not armed |
| `Executed` | The patched fields are written; the one-shot stays done | **Re-armed**: `Enabled`, firing at the new schedule's first slot |

**A paused job stays paused.** An edit is not a resume: it keeps `Disabled` and no `next_trigger_at`, and the user restarts it explicitly with `CronResume`. An edit that quietly re-arms a job the user stopped is the same class of bug as a delete that keeps firing. The new schedule is still validated on the way through, even though the paused job is not armed with it — a schedule with no fire time left would be a job that can never be resumed, and the edit is the last chance to say so.

**A fired one-shot can be rescheduled.** `Executed` + a new `At` in the future re-arms the job: status back to `Enabled`, `next_trigger_at` at that instant. This is precisely what "move that reminder to tomorrow" means, and it is why the row is preserved after a one-shot fires — the job comes back with its id, its past runs, and the conversations they opened intact.

**A job in the recycle bin cannot be edited.** `is_deleted` reads as absent, exactly as it does for `enable_job`, `disable_job` and `trigger_now`: `CronError::NotFound` → 404, telling the caller to restore it first. Restoring is the only way out of the bin.

**A fire already in flight is unaffected.** `CronExecution` snapshots `prompt` / `title` / `schedule` / `timezone` at record time and the trigger is built from that snapshot, so an edit that lands between record and dispatch does not change what that fire does or how its notification is named. The edit wins on the job row; the fire in flight runs what it was recorded with, and the next fire picks the edit up.

Three surfaces drive `update_job`: the `CronUpdate` tool, `PATCH /v1/cron/{id}` on the admin API, and the edit modal on the web cron page. All three hand back the **edited job**, not an empty acknowledgement — an edit's outcome cannot be predicted from its request (`next_trigger_at` comes out of cron parsing in the job's timezone, and whether the job re-armed at all depends on the status it was in), so returning it is what saves every caller a refetch and lets the model tell the user when the job actually runs next. The empty patch is a 400, an `At` with no future left is a 400, a binned or unknown id is a 404, and a lost race is a retryable 409 (`CronError::Contended` — nothing changed; send it again).

### Cron expressions are timezone-aware

Each `CronJob` carries an IANA `timezone` field (e.g. `"Asia/Shanghai"`, `"UTC"`). Cron expressions are evaluated **in that timezone**: `0 9 * * *` with `timezone = "Asia/Shanghai"` fires at 09:00 Shanghai time daily, not 09:00 UTC. The scheduler uses `chrono-tz` to convert the current UTC instant into the target zone, asks the `cron` crate for the next match in that zone, and converts the result back to `DateTime<Utc>` for persistence and the storage index. `At { time }` carries an absolute UTC instant and ignores `timezone`. Old rows persisted before this field existed deserialize with `"UTC"` — preserving their original behavior. The timezone also dates a recurring fire's conversation title, so a job that fires at 07:30 Shanghai is filed under its own local day.

### Schedule as a typed enum

The `schedule` field is `CronSchedule`, a tagged enum with two variants: `Cron { expr: String }` for recurring jobs and `At { time: DateTime<Utc> }` for one-shot jobs. The variant alone determines recurrence — there is no separate "run mode" — and, now, which delivery path the fire takes. Cron-expression parsing and validation happen at creation time in `CronScheduler`, not at the type level.

### The execution row is the fire's snapshot

`CronExecution` snapshots what actually ran — `prompt`, `title`, `timezone`, `schedule` — so a job edited or deleted between record and dispatch cannot change what a fire does or how its notification is named. `CronTriggerEvent` is built entirely from the execution (`CronTriggerEvent::for_execution`), which is also why the boot re-drive can rebuild a delivery payload identical to the live one.

### Fire-time framing: a fire is a task, not a user message

A fire is delivered to the model as a *user* turn, so a bare prompt is ambiguous: a job created to "say 你好 in a minute" stores the prompt `你好`, and at fire time the model reads `你好` as the user greeting it and greets back instead of performing the send. Two layers keep the intent unambiguous:

- **At creation**, the `CronCreate` tool (`baybo-cron::tools`) steers the model to write `prompt` as a self-contained, imperative *task instruction* ("Send the user a greeting: 你好") — the fire runs in a fresh session with no memory of the conversation, so every needed detail must be inlined.
- **At fire time**, the agent layer wraps `prompt` via `baybo_context::prompts::cron::frame_cron_prompt` before it reaches the LLM. The framing states that this is a scheduled fire (not a live user message), that the prompt is an instruction to carry out now and report back, and that the `[cron:<job_id>]` routing tag is diagnostic-only and must never surface in the reply. `original_cron_prompt` reverses the framing for operator previews and stays backward-compatible with legacy `[cron:<id>] <prompt>` rows.

### Every job has a title

`CronCreate` requires a `title` — the creating model names the job, having the full conversation in context. It names the conversation a recurring fire opens, heads a one-shot's notification, and labels the job in `baybo cron list` and the admin cron page. Legacy rows (title-less) fall back to a truncated prompt via `CronJob::display_title`, so no surface renders an empty name.

### LLM-invocable cron tools live in baybo-cron

`tools::agent_tools` returns `CronCreateTool`, `CronUpdateTool`, `CronDeleteTool`, `CronPauseTool`, `CronResumeTool`, and `CronListTool` `Tool` implementations (each holding an `Arc<CronScheduler>`). They live in `baybo-cron::tools` — the same pattern as `baybo-skills::tools` — so the cron domain owns its own LLM surface. This is only possible because `CronStore` moved to the `baybo-store` ports crate: the old `baybo-storage → baybo-cron` edge is gone, so `baybo-cron` taking a dependency on `baybo-tools` (for the `Tool` trait) no longer closes the cycle `baybo-cron → baybo-tools → baybo-storage → baybo-cron`. `crates/baybo/src/runtime.rs` registers them into the `ToolRegistry` after the scheduler is constructed.

The three single-job tools (`CronDelete` / `CronPause` / `CronResume`) share one `{ id }` parameter shape — one `JobIdParams`, one schema builder, one progress label — so the only thing that distinguishes them to the model is the description, and each description carries the distinction it has to get right: delete stops the job and leaves the list *but is recoverable from the recycle bin*, pause keeps the job listed and stops it *until resumed*, resume computes the next fire *from now* and cannot revive a one-shot whose moment has passed. Both of the descriptions a model reads when it is about to *replace* a job — delete's, and resume's refusal of a fired one-shot — point it at `CronUpdate` instead, because that is the moment it would otherwise reach for delete + create.

`CronUpdate` takes `{ id }` plus the fields to change, and its description carries the four things the model gets wrong on its own: **edit, do not delete + create** (that mints a new job and throws the old one's history away); a field left out **keeps its current value**, and setting none at all is an error; changing the schedule recomputes the next fire **from now**, with nothing back-filled and a past `at` refused; and a **paused job stays paused**, so `CronResume` is what starts it again. It reports the job's new `next_trigger_at` in the job's own timezone, so the model's reply can say when the job actually runs next rather than echoing the expression back.

Its schedule arrives the way `CronCreate`'s does — a recurring `schedule` expression or a one-shot `at`, **mutually exclusive**, both omitted meaning "leave the schedule alone" — because the model composes the two tools from the same mental model, and a `CronSchedule` enum on the wire is a shape it fumbles. Two details keep that surface honest: a blank string is read as a field the caller *did not set* (the model reaches for `""` to mean "leave this alone" often enough that taking it literally would rewrite the job with an empty cron expression), and a naive `at` is read **in the zone the job lives in** — the one this call sets, or else the one the job already has — because a wall-clock reminder read as UTC would move by the offset.

The bin itself is **not** part of the model's surface. `CronList` returns live jobs only: a paused job appears with `status: disabled`, a deleted one does not appear at all, so the model can neither see nor act on a job the user has removed. Restore is a human affordance — the web cron page's Recycle Bin view and `POST /v1/cron/{id}/restore`.

### Storage decoupling

The `CronStore` trait lives in the `baybo-store` ports crate (its sqlite impl in `baybo-storage`) and operates on the domain types directly — `CronJob` / `CronExecution` / `ExecutionStatus` / `ExecutionCompletion` rather than opaque row shapes. The sqlite implementation in `baybo-storage::sqlite::cron` handles JSON serialization of the `data` column internally and projects the queryable fields into columns: `status` / `next_trigger_at` / `deleted_at` on `cron_jobs` (so the listing invariant above is enforceable in SQL), and `completed_at` / `notified_at` on `cron_executions` (so the boot re-drive's scan is an indexed query rather than a full-table deserialize).

**The `deleted_at` column is the source of truth for recycle-bin state, and only `delete` / `restore` write it.** The `data` blob does not carry a copy at all — `serialize_job` strips it — so the column can never be contradicted by a stale snapshot's idea of it, and `write_deleted_at` touches the column alone (re-serializing the blob there would revert a fire's write-back that landed in between).

**A fire and a user's write race each other on the same row, in both directions.** The tick loop reads a due job, records an execution, then writes the job back with its advanced slot. An edit (`update_job`) reads a job, recomputes its schedule, then writes it back. Each one's window is wide enough for the other to land inside it, and *both* directions lose data if the write is a plain full-row `save`. So both writes are conditional on the row being **unmoved** — the `UNMOVED` predicate, `status = :expected AND next_trigger_at = :expected AND deleted_at IS NULL`. A fire, a pause, a resume and a reschedule all move one of those two columns, so a row that still carries both of the snapshot's values is a row nothing has happened to since it was read; the delete, which is deliberately orthogonal to `status` and so moves neither, is caught by the third clause. Editing needs no new column and no migration to say all this: `status` and `next_trigger_at` are already projected out of the blob for the tick query, which is exactly what makes them available as the guard.

- **A fire landing inside an edit's window.** An unconditional `save` would put the pre-fire snapshot back: re-arming a slot that already ran, forgetting `last_triggered_at`, and resetting a one-shot from `Executed` to `Enabled`. The edit therefore writes through `CronStore::save_if_unchanged(job, expected)`, and on refusal reloads and re-applies the patch to the row **as the fire left it** (three attempts, then `CronError::Contended` — the row is intact and the identical request succeeds on retry, which is why it is a 409 and not a 500). Re-applying is also what makes rescheduling a fired one-shot fall out for free: the patch lands on the `Executed` row, so a future `At` re-arms it deliberately rather than by racing.

- **An edit landing inside a fire's window** — the direction a compare-and-set alone cannot fix. A prompt-only edit moves neither column the predicate tests, so *any* CAS on the fire's write still passes; a write-back that re-serialized the whole pre-fire snapshot would silently revert that edit a millisecond after the user was told it landed, and the next fire would run the old prompt. So the fire's write-back stops carrying fields it does not own: `CronStore::record_fire(expected, CronFire { status, next_trigger_at, last_triggered_at, updated_at })` reads the row's **current** blob and stamps only those four fields onto it, inside a single `BEGIN IMMEDIATE` transaction so there is no window between that read and the write. It is the same idiom as `write_deleted_at` — touch the columns you own, never re-serialize the blob.

A fire owns when it ran and where the schedule goes next. It owns nothing the user typed. And because its write is still `UNMOVED`-conditional, a pause, a delete or a reschedule that lands mid-fire wins outright: the write-back is dropped whole rather than advancing a schedule the user has just replaced. The fire already in flight completes either way — it was legitimately due — only the row's schedule is left as the user's write left it.

Rejected: matching the blob text itself as the CAS. A stored blob that lacks a field a newer job carries (a title-less legacy row) does not re-serialize byte-identically, so its job would never advance again — a permanently dead schedule. `updated_at` as a version is the same class of fragility (an RFC3339 text comparison), and a dedicated version column would be a schema change to answer a question `status` and `next_trigger_at` already answer.

## Constraints

- No dependency on `agent` or `storage`. Depends on `baybo-tools` (for the `Tool` trait the cron tools implement) and `baybo-store` (the `CronStore` contract), so `baybo-cron` is no longer a leaf — it mirrors `baybo-skills`, which also carries its own `tools` module. No cycle: nothing in `baybo-tools`'s dependency graph reaches back to `baybo-cron`.
- Depends on: `baybo-model`, `baybo-store`, `baybo-tools`, `chrono`, `chrono-tz`, `cron`, `tokio`, `parking_lot`, `serde`, `serde_json`, `uuid`, `async-trait`, `thiserror`, `anyhow`, `tracing`
- `test_support::InMemoryCronStore` (behind the `test-support` feature) backs the scheduler's own tests and the agent layer's delivery tests.

## Collaboration

| Module | Role |
|--------|------|
| `storage` | `SqliteCronStore` implements the `CronStore` trait (from `baybo-store`) against sqlite, over `baybo-model` types; no dependency on `baybo-cron` |
| `tools`   | `baybo-cron::tools` implements the `Tool` trait (`CronCreate` / `CronUpdate` / `CronDelete` / `CronPause` / `CronResume` / `CronList`), bridging `Arc<CronScheduler>` to the registry; `crates/baybo/src/runtime.rs` registers them |
| `agent`   | Re-exports `CronScheduler` / `CronTriggerEvent`; `Router` consumes the event stream, mints the fire session, routes `AgentMessage::CronTrigger`, waits on one-shot fires, and re-drives undelivered results at boot. The origin actor handles `AgentMessage::CronResultReady` |
| `job`     | `JobInput::Cron` marks a fire; `JobInput::CronNotification` marks the (inference-free) delivery of a one-shot's result, whose `Completed { reply_ordinal }` edge drives push |
| `gateway` | Lists conversation-marked cron sessions in the chat list; pushes `Cron` completions for conversations and every `CronNotification`; the admin `/cron` surface lists (`?deleted=true` serves the recycle bin), inspects (live or deleted), creates, edits (`PATCH`, answering with the edited job), pauses, resumes, deletes and restores jobs |
