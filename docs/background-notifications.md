# Background-job notification pipeline

This document covers proactive delivery of terminal results from detached
subagents and detached `Bash` commands into their parent conversation. It does
not cover cron-result delivery, progress notices, or background context
summarisation; those are separate pipelines with different delivery contracts.

## Vocabulary and ownership

| Concept | Meaning | Owner |
|---|---|---|
| In-flight job | Detached work that has not reached a terminal state | Supervisor registry + job lifecycle |
| Group | A barrier cohort waiting for all grouped subagents, or its timeout | `BackgroundNotificationState::groups` |
| Buffered result | A terminal result not yet committed to the transcript | `BackgroundNotificationState::buffered_results` |
| Active delivery | One batch whose prompt is already durable but whose proactive reply has not settled | `BackgroundNotificationState::active_delivery` |
| Passive delivery | No more autonomous retries; the durable prompt remains visible to the model on the next real user turn | Transcript |

The durable state is one aggregate on `SessionState`:

```rust
BackgroundNotificationState {
    groups: HashMap<String, BackgroundNotificationGroup>,
    buffered_results: Vec<PendingBackgroundResult>,
    active_delivery: Option<BackgroundNotificationDelivery>,
}
```

The aggregate is `serde(flatten)`ed. Persisted session JSON therefore retains
the historical keys `background_groups`, `pending_background_results`, and
`pending_notification_turn`; existing rows need no migration. The Rust names
describe the current concepts instead of exposing the storage history.

`buffered_results` and `active_delivery` are intentionally not an enum. They
may both be populated: while an older batch is retrying, newly completed jobs
accumulate in the buffer as the next batch.

## End-to-end flow

```text
detached job running
        |
        | terminal result (BackgroundJobFinished)
        v
group barrier, when present -------- timeout / complete
        |                                      |
        +--------------------------------------+
                         v
                  buffered_results
                         |
                         | no higher-priority mailbox work
                         v
             build + append hidden prompt row
                         |
                         | newly inserted (not a crash replay)
                         v
       persist + dispatch acknowledgement control event
                         |
                         v
                  active_delivery
                    |
                    +-- streamed reasoning / tools / answer deltas
                    |
                    +-- non-blank success --> clear ledger --> final Message
                    |
                    +-- blank success -----> clear ledger --> no final Message
                    |
                    +-- failure below cap -> persist count -> timer/backoff
                    |                                  |       + re-anchor if superseded
                    |                                  +------> active_delivery
                    |
                    +-- fifth failure ------> clear ledger --> passive
                    |
                    +-- successful user turn -> clear ledger --> passive
```

### 1. Completion intake

The detached wait task posts `AgentMessage::BackgroundJobFinished` to the
parent actor. `/stop` removes still-running subagents from the supervisor's
in-flight registry; their wait tasks observe the missing entry and suppress the
terminal event. Results that completed before `/stop` remain valid and are not
discarded. Process shutdown is also a suppressing cancellation edge for
detached commands: the command escort kills the child, clears its registry, and
never publishes `BackgroundJobFinished`. The partial stdout/stderr artifacts
remain available for inspection. A command terminated by Baybo shutdown is not
a failed task result owed to the user.

The actor deduplicates `handle_id` across all three durable collecting/delivery
locations: buffered results, group members, and the active delivery's
`handle_ids`. A result is routed into its still-existing group or directly into
the buffer, and the session row is saved immediately. The buffer is capped at
64 entries with drop-oldest semantics; the full result remains available in
the child transcript or command output file.

### 2. Group barriers

Each grouped spawn is counted under a key namespaced by the dispatching turn's
`TurnId`. At the turn boundary the cohort seals, fixing its membership and
starting its 30-minute timeout.

- A sealed cohort with every member terminal releases immediately.
- An incomplete cohort releases its finished members when the timeout expires.
- The timed-out cohort is removed. A straggler that finishes later finds no
  group and enters the ordinary buffer, so it can notify independently.

Open groups count as outstanding notification work, keeping the actor on the
timer path even when no result is buffered yet.

### 3. Buffer-to-delivery durability boundary

Once the mailbox has no queued user trigger or background completion, the actor
takes the entire buffer as one batch. `baybo-context` builds both the
user-facing acknowledgement and the hidden `<background_results>` analysis
prompt.

The ordering is load-bearing:

1. Derive the batch's stable operation key from the sorted handle IDs:
   `background-notification:<batch-hash>:prompt`.
2. Atomically append the hidden agent-context prompt to `session_messages`.
   If the append fails, restore the untouched batch to `buffered_results`.
3. If the prompt key already exists after a crash replay, recover its ordinal without
   duplicating the row. If that historical row was superseded by compaction,
   re-anchor it under an operation-specific key first.
4. Only when step 2 **inserted** a new row, persist the acknowledgement as a
   `NoticeInfo` control event anchored after the prompt row, and dispatch it as
   an `AgentEvent::Notice` carrying that event's durable id. It is a bland
   "work finished, reviewing it now" line with no result content.
5. Open `active_delivery` with `handle_ids`, frozen `content`,
   `prompt_ordinal`, and `failed_attempts = 0`.
6. Persist the single session-state transition: buffer emptied, delivery
   opened.
7. Only after that save succeeds, run the inference turn with the actor's
   response channel as `delta_tx`.

**The acknowledgement is a control event, not a transcript row.** It used to be
an ordinary assistant row in `session_messages`, which put a fixed sentence into
the model's own context as its last words — a template the model imitated,
appending the acknowledgement verbatim to the end of its later answers, so the
user saw the same line twice in a row. Instructing the model not to repeat it
cannot fix that: the imitation carries into ordinary user turns, which never see
the notification framing. `session_control_events` is the plane for exactly this
— shown in the chat transcript, structurally incapable of reaching the model,
with no filter for a future `session_messages` reader to forget.

**The acknowledgement carries no idempotency key of its own**; it fires only on
the prompt insert. One acknowledgement per analysis turn is precisely the rule,
and the prompt row is already the durable record of "this batch is new", so a
crash replay recovers it as `Existing` and stays quiet. A separate key hashed
over the batch's handle set could not express that rule: a result joining the
batch between a failed attempt and its retry changed the hash and produced a
second acknowledgement for the same work.

From step 2 onward the prompt row is the durable copy of the result batch, so
raw results do not need to be re-buffered after an inference failure. A crash
between the transcript append and the session-state save restores the raw batch
on hydration, but its deterministic key resolves to the row already written.

A control-event write is best-effort: if it fails, the acknowledgement is missing
from a later reload, never the report.

The inference turn retains the historical persisted/API kind
`SubagentNotification` for compatibility, although its payload may describe
either subagents or commands.

### 4. Streaming analysis and delivery outcomes

The acknowledgement itself runs no inference. The following parent-agent turn
is fully streaming: model reasoning is emitted as `AgentEvent::Reasoning`, tool
lifecycle and progress events use their normal variants, and answer prose is
emitted as `AgentEvent::AnswerDelta`. Channel surfaces retain their own display
policy — for example, Web renders reasoning while the TUI intentionally shows
its working indicator instead of raw reasoning text.

- Non-blank success: clear and persist `active_delivery`, then dispatch the
  canonical final `OutgoingMessage`.
- Blank success: clear and persist the delivery, but suppress the final
  message. The earlier acknowledgement remains visible.
- Failure: increment `failed_attempts`, persist it, and leave the delivery
  active for the timer.

The actor starts with a 60-second retry delay and doubles it after each timer
attempt up to 300 seconds. Any inbound actor message resets the schedule.
Retries happen only on this timer; the post-message drain never retries an
active delivery immediately.

### 5. Forward-only retries

Retries never roll transcript history back. A failed turn's partial rows are
real history explained by the original prompt.

Before retrying, the actor verifies that `prompt_ordinal` still belongs to the
active transcript:

- If active, nothing is appended. Should a prior cancelled attempt have left an
  assistant salvage row at the tail (a request ending on an assistant message
  is provider prefill, which Anthropic rejects with extended thinking on), the
  **request-time retry cue** supplies a synthetic user-role tail. The cue is not
  a transcript row: it is applied by `ContextManager` as a request suffix only
  while the tail is actually an assistant row, recomputed per request and
  carried on the trace marker so replay matches the request. It is never
  persisted — an earlier persisted, attempt-keyed cue was a no-op on the exact
  crash-replay it had to survive (the counter only advances on an *observed*
  failure, which a crash skips) and left permanent rows in the append-only log.
- If compaction superseded the row, append the frozen `content` again and
  update `prompt_ordinal` before retrying. The re-anchor key includes the prior
  prompt ordinal, making that append idempotent without colliding with the
  original historical row.

Store failures while verifying or re-anchoring count against the same attempt
cap as inference failures. Otherwise a partially broken store could keep
refreshing `last_active` and pin the actor resident forever.

After five failures, the actor clears the ledger and degrades to passive
delivery. The prompt row remains in the transcript, so the next real user turn
can report it.

### 6. Interaction with user turns and fresh completions

User messages have higher mailbox priority than background completions. If a
user turn completes successfully while `active_delivery` is open, that turn's
request already included the persisted notification prompt. The actor clears
the delivery as passively settled instead of scheduling a duplicate proactive
retry. A stopped or blank user turn does not settle it.

New terminal results arriving while a delivery is active enter
`buffered_results`. They cannot start until the active delivery succeeds or
becomes passive. This gives the pipeline a single in-order delivery head and a
mergeable tail.

### 7. Eviction and crash stance

Every aggregate transition is saved on the session row, so idle actor eviction
does not lose notification work. Hydration restores the transcript and the
aggregate, then resumes the timer path.

`ActorStop` performs a final state save. This heals a transient failure to
persist a just-cleared delivery ledger without rewriting `last_active`; an idle
conversation must not jump to the top of the chat list merely because its
in-memory actor was reaped.

The prompt and re-anchor appends are idempotent around process crashes; the
retry cue needs no key because it is never persisted, and the acknowledgement
needs none because it fires off the prompt insert. External dispatch is not in
the same transaction: a crash after the acknowledgement's control-event insert
but before its channel send leaves the durable event for history sync, while a
crash after final-report channel delivery but before the active-ledger clear can
duplicate that report on retry. The explicit 64-entry admission cap also remains: overflow evicts the
oldest pending notification with a warning, while the authoritative child
transcript or command output remains intact.

## Invariants

- Session rows and transcripts are never deleted by this pipeline.
- At most one `active_delivery` exists, but it may coexist with buffered/grouped
  results for later batches.
- Raw results own analysed-report delivery before the hidden prompt append; the
  prompt row and ledger own it afterward. Every crash-replayable synthetic
  append to `session_messages` carries a stable operation-scoped source-event
  key.
- Nothing this pipeline shows the user is also shown to the model as its own
  words. The acknowledgement lives on `session_control_events`; only the
  analysed report the model actually produced is a transcript row.
- An active delivery always finishes or degrades before the next buffer drains.
- Retry is append-only and forward-only; no transcript rollback occurs.
- Mailbox priority is ordering, not preemption. `/stop` is the only hard
  interrupt path.
- Notification framing is per-turn content, not part of the system prompt, so
  the normal prompt-cache prefix remains stable.
