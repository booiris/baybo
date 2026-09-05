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

The batch size is whatever the buffer holds — a fan-out whose members finish
while a user turn runs arrives as one batch, not as one turn each. So the
framing is chosen by that size. One result keeps the historical sentence, which
asks for "the useful outcome". Two or more get `BATCH_FRAMING`, which states
the count, repeats it in the block's `count` attribute, and requires one
numbered Markdown section per `<result>`, headed by that result's one-based
index and `<task>` **verbatim**. The acknowledgement also states the exact
plural count, so the framing never claims the user saw a number it did not
receive. The numbered verbatim heading is what the coverage audit in §4
measures; without it the audit could only guess at paraphrase, and identical
labels could not be distinguished. It stops short of claiming an omitted
result is lost, because it is not: the prompt row stays in the transcript,
retry re-anchors it past a compaction, and every result names a readable path
to its full text.

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

#### Result bodies, and where the rest lives

Each `<result>` element's `<output>` is capped at `MAX_RESULT_BYTES` —
defined as `MAX_TOOL_OUTPUT_BYTES`, deliberately the same budget the *same*
report would have kept had the job finished inside its foreground wait and
returned as a tool result. The two paths must agree: a foreground subagent
that crosses `SUBAGENT_FOREGROUND_WAIT` converts to background and delivers
here instead, and when this cap was smaller that conversion silently cost a
report ~30x more of itself than finishing a second earlier would have. One
turn's combined bodies are additionally bounded by `MAX_BATCH_BYTES`, split
evenly, so draining a full 64-result buffer cannot land a multi-megabyte
prompt; at real batch sizes each result still gets the whole per-result
budget.

A body that *is* cut names the absolute path holding the full text, and both
kinds carry that path as an element beside the body:

- subagent → `<transcript_file>` (`<root>/logs/sessions/<child-id>.jsonl`,
  served out of the store by `SessionTranscriptReader` — nothing is written
  there, and `Read` resolves it before touching the filesystem)
- command → `<output_file>`

The path is the load-bearing part. `<child_session>` alone affords only a
resume, which spends an LLM round-trip re-dictating text that is already
sitting in `session_messages` — and a resume can itself be killed at the
foreground wait, which is how a truncated batch once reached the user with the
missing table still missing.

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

Success is "the turn returned", not "the report was complete". `is_blank_reply`
is the only content gate, so a reply covering one result of three settles all
three, and `failed_attempts` never moves — the attempt cap governs whether an
answer arrived at all, never whether it answered everything. Before clearing a
batch the actor therefore audits it: `unreported_result_indices` returns the
one-based result indices whose required heading is absent, and a non-empty
result logs a warning with those indices. Labels are deliberately excluded from
the log because a detached command's label *is* its command line, which
routinely carries credentials, and logs are served over an HTTP endpoint.

The heading match is looser than the framing's `## <index>. <task>`: any
heading level, any punctuation after the number, and any emphasis count. A
tripwire that fires on well-formed reports is worse than one that misses,
because the reader learns to skip it and the real partial report goes unnoticed
with it. What stays required is that the line be a heading (prose mentioning
the task is not the requested section) and that the number open it (`## 11.`
cannot answer for result 1).

The task text is required on that line — in either its raw or XML-escaped
spelling — only when a heading could hold it at all. A subagent's label is its
one-line description, so it almost always can. A detached `Bash` job's label is
its **command line**, stored uncapped and unflattened, so a heredoc or a
`&& \`-continued pipeline arrives with newlines in it; a Markdown ATX heading
is single-line by definition, and demanding one anyway would fire the tripwire
on every well-formed report in that batch. Such a result is carried by its
index alone, and the framing tells the model to shorten a long or multi-line
task in the heading rather than attempt it.

Both proactive notification turns and successful user turns that passively
settle an open delivery run the same audit at the same level. The passive case
is not the routine one it looks like: a ledger opens and clears inside a single
actor message, so a user turn can only interleave with a delivery that failed,
was cancelled, or was cut short by a restart — never with a healthy one. What
the warning claims there is that the batch is retiring without a *complete*
report, not that nothing reached the user: `/stop` is deliberately not counted
as a delivery failure, and the turn it stopped had already been streaming, so
its earlier sections may have been read. Observation only — the
delivery still settles. Narrowing a partial batch
into a follow-up delivery needs a fresh prompt rendered from the un-named
results under its own source-event key, because `handle_ids` is simultaneously
the crash-replay idempotency key and the terminal-event dedup set, and
`content` freezes the whole batch; trimming either in place would resend what
was already reported and break replay. A batch of one is never audited: its
framing asks for no verbatim heading, so it cannot be short one. Ledgers
written before `result_labels` existed deliver but do not audit.

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
- A result body the prompt truncates always names a readable absolute path to
  the whole of it. Nothing this pipeline drops is unreachable.
- The background body budget never falls below what the same result would have
  kept on the foreground tool-result path; crossing the foreground wait changes
  *when* a report arrives, not how much of it survives.
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
- The framing that imposes a coverage duty and the predicate that audits it
  live in the same module, so an instruction the report is not held to, or a
  check for something never asked, is a single-file edit away from being
  caught. Every successful reply that settles a batch runs the check first;
  failure-cap degradation has no reply to audit.
