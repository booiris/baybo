# The ask prelude as a handle

> **Status:** deferred, not started. The rule it protects is enforced today
> by a token parameter; this is a cleaner shape for the same guarantee.

## What exists today

Every door that appends to a transcript has to settle the session's
unapplied asks first, or its row lands after a `tool_use` that has no result
— an order no provider accepts, and nothing repairs before the next LLM
call. See "The turn prelude" in
[`../modules/human-ask.md`](../modules/human-ask.md).

The compiler keeps doors on that rule through a proof token:

- `AgentLoop::settle_before_append` (`crates/agent/src/runtime/agent_loop/settle.rs`)
  runs the prelude and returns `AsksSettled`, a zero-sized value only that
  module can construct.
- The door appends on `AgentLoop` — `append_user_message_with_platform_msg_id`,
  `append_issue_brief`, `append_cron_fire`, `append_cron_notification`,
  `append_background_notification_prompt_once` — and `compact_now` take
  `_settled: &AsksSettled` as their first parameter.

It works, and a door that skips the prelude does not compile. Its weaknesses:

- The parameter is never read. Six functions carry an argument whose only
  job is to exist, and its spelling (`pub struct AsksSettled(())`) needs
  explaining to every new reader.
- The token proves the prelude ran, not that it ran just before this append.
  A door can hold it across other work.

## The proposal

Make the appends unreachable without the prelude instead of guarded by a
token: `settle_before_append` returns a handle that borrows the loop, and the
door appends become methods on that handle.

```rust
/// The transcript of a session whose unapplied asks have been settled.
/// The only way to append a door's input row.
pub struct SettledTranscript<'a> {
    agent_loop: &'a mut AgentLoop,
}

impl AgentLoop {
    pub async fn settle_before_append(&mut self, /* session, lifecycle, … */)
        -> anyhow::Result<SettledTranscript<'_>>;
}

impl SettledTranscript<'_> {
    pub async fn append_user_message(&mut self, content, platform_msg_id) -> anyhow::Result<()>;
    pub async fn append_issue_brief(&mut self, number, checkout, brief, files) -> anyhow::Result<()>;
    pub async fn append_cron_fire(&mut self, job_id, prompt, context) -> anyhow::Result<()>;
    pub async fn append_cron_notification(&mut self, content, source_event_id) -> anyhow::Result<…>;
    pub async fn append_background_notification_prompt_once(&mut self, content, source_event_id) -> anyhow::Result<…>;
}
```

`AsksSettled` and the six `_settled` parameters go away. The prelude still
runs once per door, as a visible step, and the borrow ends the handle before
the door's own turn starts, so it cannot be kept and reused later.

## What makes it more than a rename

- **The borrow.** The handle holds `&mut AgentLoop`, and the doors reach the
  loop through `self.volatile.agent_loop` while also touching
  `self.durable.session` and other actor fields between appends. Each door
  has to be arranged so the handle's borrow covers only its appends.
- **Background notifications.** That path appends at three sites spread over
  `run_background_notification_delivery` and
  `retry_background_notification_delivery`
  (`crates/agent/src/actor/background_notification.rs`), with session-state
  writes and store reads in between. It either threads the handle through
  both functions or is restructured so the appends sit together.
- **`compact_now`.** It appends nothing but its summariser reads the
  transcript, and it opens its own turn with the session and lifecycle. It
  can become a method on the handle, or stay on `AgentLoop` and take the
  handle by reference (which keeps one token-style parameter).
- **`append_cron_notification`.** It is called inside the `CronNotification`
  turn's `with_turn` closure, after the prelude ran outside it. The handle has
  to be created before that turn opens and moved into the closure.

`append_spawned_prompt` is unaffected: it opens a subagent's fresh session,
which has no asks.

## Not this

Do not move the settlement *into* each append. The settlement opens its own
turn and can re-execute an approved tool, so every append would need the
session, turn lifecycle, span recorder, tokens and output channel; a function
named `append_…` would sometimes run a tool for minutes; and
`append_cron_notification` and `compact_now` would open the settlement turn
inside their own.
