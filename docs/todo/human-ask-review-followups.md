# Human-ask restart survival: review follow-ups

> **Status:** Item 1 remains deferred from the structural review of PR #426.
> Items 2–4 are complete.

See [`../modules/human-ask.md`](../modules/human-ask.md) for the design these
refer to. The deferred reshaping of the prelude's proof token has its own
note: [`ask-prelude-handle.md`](ask-prelude-handle.md).

## 1. Re-execution is a thinner copy of the batch's dispatch

When the user approves a call nothing was waiting on, the settlement runs
that call: `AgentLoop::reexecute`
(`crates/agent/src/runtime/agent_loop/settle.rs`). It hand-builds the same
22-argument `ToolExecutor::execute` call as the batch in
`AgentLoop::run_iteration` (`crates/agent/src/runtime/agent_loop.rs`), with
`PreExecuteGate::PreDecided(decision)` in place of `PreExecuteGate::Ask`.
Only the tail is shared (`append_executed_tool`). What the batch does around
the call, and `reexecute` does not:

- **It does not race the cancel.** The batch waits on the call futures and
  the turn's cancel token together and drops the unfinished futures.
  `reexecute` awaits the call directly, and neither `with_step` nor
  `with_turn` races the token. So:
  - a `/stop` on a re-executed tool that ignores its token waits for the
    tool's own timeout;
  - a re-executed call that raises a during-execute approval and then meets
    an ancestor cancel (shutdown, or the idle reaper's release) never
    returns, because the gate's `Ancestor` arm is `pending()` and relies on
    the loop dropping the future. The actor cannot end that turn.
- **It does not claim an interrupted call.** After a cancel the batch calls
  `claim_interrupted_call` (→ `HumanAskManager::abandon_for_tool_call`)
  before appending. The settlement passes the `ExecutedTool` straight on and
  never reads `.abandoned`, so an approval raised during re-execution and
  then stopped stays pending and unapplied until a later settlement or the
  next boot reconciles it.
- **It runs one call at a time.** Several approved calls of one batch are
  re-executed serially, without the permit pool. Slower, not wrong.

**Fix.** One dispatch path used by both. Extract from `run_iteration` a
private function that owns the permit pool, the single `executor.execute`
call site, the `FuturesUnordered` + cancel race + drop, and the
`CancelOrigin` read; and a second that does the per-call post-processing
(`claim_interrupted_call`, the cancelled fill, `append_executed_tool`). The
settlement calls the same two with `PreDecided`. It must keep ledger order
(synthetic denials interleaved with executed results) and must not call
`ReadTracker::begin_response` on the batch's behalf.

**Why deferred.** It moves about 150 lines of the path every turn takes.
Permit acquisition, the `approved_resources` write-back and
declaration-order appends all have to survive the move, or prompt-cache
stability and `tool_use` / `tool_result` pairing break for every turn, not
only for resumed ones. It deserves its own PR and review.

## Completed follow-ups

2. The SQLite restart suite registers the actor with the supervisor, runs
   `reap_idle`, waits for the actor to exit, then answers through the real
   router and verifies exactly one Resume turn and tool execution.
3. The restart and settlement suites share their gated tool and transcript,
   turn-count and reply helpers in `crates/integration-tests/src/human_ask.rs`.
4. The loop returns the cancellation origin it observed. A later release
   preserves the mailbox behavior of an observed user stop. When a
   non-resumable approval's timeout write fails, the gate waits for any
   answer already claiming the row before closing the waiter.
