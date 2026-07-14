# cron - Cron Jobs and Scheduler

## Overview

The `cron` crate owns scheduled recurring work end-to-end: the `CronScheduler` (`scheduler.rs`) that ticks against the store, the `Shutdown` trait (`shutdown.rs`) used to bound the scheduler's tick loop, and `CronError`. The cron data types (`CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `ExecutionStatus`, `ExecutionOutcome`, `PendingCronResult`) live in `baybo-model` (re-exported here for back-compat); the `CronStore` persistence trait lives in the `baybo-store` ports crate. It uses standard cron syntax (5-field expressions normalized to 6-field for the `cron` crate) for recurring jobs and an absolute UTC instant for one-shot jobs. The sqlite implementation of `CronStore` lives in `baybo-storage`; the LLM-invocable cron tools (`CronCreate` / `CronDelete` / `CronList`) live in `baybo-cron::tools` (the crate depends on `baybo-tools` for the `Tool` trait). `baybo-agent` re-exports `CronScheduler` and `CronTriggerEvent` for assembly-layer consumers.

CronJobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration. Each fire mints a brand-new session in the agent layer — one trigger = one session — so the run sees a clean transcript and fresh `SessionState`. A `CronJob` also records its `origin_session_id`: the conversation it was created from, which for a one-shot is where the fire's result is reported back.

## Where a fire's result goes

The two schedule shapes have genuinely different notification stories, and the design leans into that rather than papering over it. Both are **deterministic**: no LLM inference sits in the delivery path, so the model can never digest a scheduled reminder into silence, and each fire costs exactly one inference.

### Recurring (`CronSchedule::Cron`) — the fire opens a conversation

Every fire's fresh session is a **first-class conversation**: `TriggerSource::Cron { conversation: true }`, titled `{job title} · {M/d}` (the fire's date in the job's own timezone), listed in the chat sidebar, replyable, pinnable, and pushable. Being replyable has two consequences for its actor, and both are load-bearing: it is **registered** with the supervisor (an unregistered actor would let a reply fork a second actor over the same transcript), and it is **not stopped** when the fire ends (it is reclaimed by the idle reaper, like every other conversation's). Stopping it immediately would be worst exactly when it matters most — the moment after a notification lands is when a user is most likely to reply, and a reply that raced the stop would be routed into a mailbox nobody is reading, reported as delivered, and dropped. The fire's reply dispatches out through the channel exactly as before — the conversation *is* the notification, and the user can follow up in it ("expand on the third item"), because the fire's framed prompt and its reply are both right there in the transcript.

The next fire still mints a **new** conversation; a fire never appends to a previous one (see the clean-transcript decision below).

A fire that produces no reply — it failed, or it ran and said nothing — would otherwise leave a conversation that is empty when opened and, having dispatched nothing, never announced itself at all (clients learn a new conversation exists from the activity pulse the gateway derives from *channel dispatch*). So those outcomes publish the **same framed notification row** a one-shot delivers to its origin: `⏰ Scheduled task "…" failed:` or `It ran, but produced no output.`

There is **no anti-flood mitigation**: a `*/30 * * * *` job opens 48 conversations a day. Daily/weekly jobs — the realistic case — cost a handful of rows a week, and a user tired of a noisy job deletes it. If that turns out to be wrong, the fix is to auto-file each job's fires into a per-job chat folder (`sessions.folder_id` and the folder tree already exist), not to collapse the fires into one thread.

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

Jobs whose `schedule` is `CronSchedule::At { time }` transition to `CronStatus::Executed` after firing — the row is preserved (not deleted), so the web UI and history queries can still see "this fired and is done". `next_trigger_at` is cleared and `last_triggered_at` is stamped at the same time. The `list_due` query filter (`status = 'enabled'`) keeps `Executed` jobs from being re-fired by the tick loop. A `CronExecution` record is persisted alongside the status update; explicit `delete_job` is still available for callers that want to remove the row.

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

`tools::agent_tools` returns `CronCreateTool`, `CronDeleteTool`, and `CronListTool` `Tool` implementations (each holding an `Arc<CronScheduler>`). They live in `baybo-cron::tools` — the same pattern as `baybo-skills::tools` — so the cron domain owns its own LLM surface. This is only possible because `CronStore` moved to the `baybo-store` ports crate: the old `baybo-storage → baybo-cron` edge is gone, so `baybo-cron` taking a dependency on `baybo-tools` (for the `Tool` trait) no longer closes the cycle `baybo-cron → baybo-tools → baybo-storage → baybo-cron`. `crates/baybo/src/runtime.rs` registers them into the `ToolRegistry` after the scheduler is constructed.

### Storage decoupling

The `CronStore` trait lives in the `baybo-store` ports crate (its sqlite impl in `baybo-storage`) and operates on the domain types directly — `CronJob` / `CronExecution` / `ExecutionStatus` / `ExecutionCompletion` rather than opaque row shapes. The sqlite implementation in `baybo-storage::sqlite::cron` handles JSON serialization of the `data` column internally; the delivery ledger additionally projects `completed_at` / `notified_at` into columns so the boot re-drive's scan is an indexed query rather than a full-table deserialize.

## Constraints

- No dependency on `agent` or `storage`. Depends on `baybo-tools` (for the `Tool` trait the cron tools implement) and `baybo-store` (the `CronStore` contract), so `baybo-cron` is no longer a leaf — it mirrors `baybo-skills`, which also carries its own `tools` module. No cycle: nothing in `baybo-tools`'s dependency graph reaches back to `baybo-cron`.
- Depends on: `baybo-model`, `baybo-store`, `baybo-tools`, `chrono`, `chrono-tz`, `cron`, `tokio`, `parking_lot`, `serde`, `serde_json`, `uuid`, `async-trait`, `thiserror`, `anyhow`, `tracing`
- `test_support::InMemoryCronStore` (behind the `test-support` feature) backs the scheduler's own tests and the agent layer's delivery tests.

## Collaboration

| Module | Role |
|--------|------|
| `storage` | `SqliteCronStore` implements the `CronStore` trait (from `baybo-store`) against sqlite, over `baybo-model` types; no dependency on `baybo-cron` |
| `tools`   | `baybo-cron::tools` implements the `Tool` trait (`CronCreate` / `CronDelete` / `CronList`), bridging `Arc<CronScheduler>` to the registry; `crates/baybo/src/runtime.rs` registers them |
| `agent`   | Re-exports `CronScheduler` / `CronTriggerEvent`; `Router` consumes the event stream, mints the fire session, routes `AgentMessage::CronTrigger`, waits on one-shot fires, and re-drives undelivered results at boot. The origin actor handles `AgentMessage::CronResultReady` |
| `job`     | `JobInput::Cron` marks a fire; `JobInput::CronNotification` marks the (inference-free) delivery of a one-shot's result, whose `Completed { reply_ordinal }` edge drives push |
| `gateway` | Lists conversation-marked cron sessions in the chat list; pushes `Cron` completions for conversations and every `CronNotification`; the admin `/cron` surface lists and inspects jobs |
