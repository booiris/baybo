# Human-ask restart survival: review follow-ups

> **Status:** All four follow-ups from the structural review of PR #426 are complete.

See [the human-ask design](../modules/human-ask.md) for the turn prelude.
The [transcript handle](ask-prelude-handle.md) implements the follow-up to the original proof token.

## Completed follow-ups

1. Live calls and approval re-executions share the permit pool, cancellation
   race and ordered result handling in `runtime/agent_loop/dispatch.rs`.
   Settlement claims an approval after acquiring its permits and before the
   tool runs. Only a fresh model response advances the read tracker.
2. The SQLite restart suite registers the actor with the supervisor, runs
   `reap_idle`, waits for the actor to exit, then answers through the real
   router and verifies exactly one Resume turn and tool execution.
3. The restart and settlement suites share their gated tool and transcript,
   turn-count and reply helpers in `crates/integration-tests/src/human_ask.rs`.
4. The loop returns the cancellation origin it observed. A later release
   preserves the mailbox behavior of an observed user stop. When a
   non-resumable approval's timeout write fails, the gate waits for any
   answer already claiming the row before closing the waiter.
