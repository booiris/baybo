# Human-ask restart survival: review follow-ups

> **Status:** deferred, not started. Left open by the structural review of
> the restart-survival PR (#426). Everything else that review confirmed was
> fixed in that PR.

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

## 2. The reaper and the router's Resume door are not driven end to end

`crates/integration-tests/tests/human_ask_restart_e2e.rs` tests the release
of a parked turn by calling `AgentSupervisor::release_parked_turn` directly,
and the harness (`crates/integration-tests/src/harness.rs`) routes to the
actor's mailbox itself: the actor is not in the supervisor's registry, and a
stand-in forwards resume requests in place of `Router::handle_resume_ask`.
Each piece has its own test (`reap_idle` and the router door in
`baybo-agent`'s unit tests), but the chain the gateway runs —
`reap_idle` → `ActorStop` queued → release → turn ends → actor exits →
answer → router hydrates a fresh actor → Resume turn — is never run as one.
Three bugs fixed in #426 lived in exactly that gap.

**Fix.** Register the harness actor with the supervisor
(`register_if_absent`), drive the test through
`supervisor.reap_idle(&session_manager, Duration::zero())`, and route the
answer through a real `Router`. `release_parked_turn` can then lose its
`pub`.

## 3. The settle and restart suites duplicate their helpers

`human_ask_settle_e2e.rs` and `human_ask_restart_e2e.rs` each carry their own
gated write tool with its `manifest()`, plus `transcript()`,
`adjacent_result()`, `resume_turns()` and `reply()`. Move them to one module
under `crates/integration-tests/src/` (the crate's lib is already its shared
fixture home).

## 4. Two narrow races

Both need two events in the same instant; neither loses data.

- **A `/stop` and a release on the same turn.** The loop reads
  `CancelOrigin` once when it sees the cancel; `run_agent_loop`
  (`crates/agent/src/actor/mod.rs`) reads it again after the loop returns to
  decide whether to discard queued messages. A release (or shutdown) landing
  between the two turns a real `/stop` into "not stopped", so messages the
  user meant to drop run. Fix: have the loop return the origin it acted on
  instead of re-reading the token.
- **A failed timeout write and an answer.** When a during-execute approval's
  `TimedOut` claim fails to write and the gate removes the queue entry just
  before a human answer claims the row, the ledger records an approval the
  tool never received (the tool already got its timeout). The reverse order
  is handled (the answer is delivered). Fix: have `HumanAskManager::resolve`
  treat a non-resumable ask whose queue delivery found no waiter as closed,
  or have the gate take the entry off the queue before attempting the claim.
