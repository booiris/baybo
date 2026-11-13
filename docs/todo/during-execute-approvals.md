# During-execute approvals

> **Status:** partly shipped. A during-execute approval now expires after
> `DURING_EXECUTE_APPROVAL_TIMEOUT` (30 min). Moving three of the four
> during-execute prompts to before-execute is planned, not started.

## The two phases

An approval is raised in one of two phases (`AskPhase`):

- **Before execute** — the executor's pre-execute gate, before the tool runs.
  If the tool future goes away, the persisted `tool_use` can be dispatched
  again with the decision already made (`PreExecuteGate::PreDecided`), so the
  ask is **resumable**.
- **During execute** — a tool that is already running asks through
  `ApprovalHandle`. `resumable()` (`crates/model/src/human_ask.rs`) treats
  every such approval as **not resumable**: the prompt is a suspension point
  inside `execute()`, and that future is the only thing that can carry on
  from it.

When the tool future of a during-execute approval goes away — a gateway
restart, an actor panic, `/stop` — the ask is abandoned: the card disappears
and the call's result is the abandoned refusal. The idle reaper never ends
such a turn (`ParkedOn::NotResumable`), because ending it would abandon the
prompt.

## Shipped: a deadline on during-execute approvals

Before-execute approvals and questions wait with no timeout; a dead one is
answered later through the Resume door. A during-execute approval cannot be,
so without a deadline it would hold its turn — actor, transcript, tool
future — in memory for as long as nobody answers.

`ChannelAskGate` therefore races a non-resumable ask against
`DURING_EXECUTE_APPROVAL_TIMEOUT`. On expiry it claims `AskResolution::TimedOut`
on the still-pending row (an answer that won the claim is taken instead),
closes the card, and returns `ApprovalResolution::TimedOut`. The tool future
is still alive, so the tool returns its own denial and the turn carries on —
**nothing is replayed**. The deadline also bounds how long the idle reaper
keeps such a session.

## Who raises during-execute approvals

| Prompt | Where | Has anything run when it asks? |
|---|---|---|
| Bash `require_escalated` | `request_explicit_unsandboxed_approval`, right after the route is picked | No |
| Bash risk judge (`permission = auto`) | `pre_exec_gate`, after only env preparation (secret env, commit trailer, git config) | No |
| Skill env vars | `request_uncached`, after the host-env check, before the skill gets them | No |
| Bash retry after a sandbox failure | `request_unsandboxed_retry_approval` | **Yes** — the sandboxed attempt ran |

`permission = manual` Bash already declares its commands through
`accessed_resources`, so those prompts are before-execute.

## Plan: move the first three to before-execute

Nothing has run at those three points, so re-dispatching the call with the
decision applied is as safe as for any before-execute approval. The executor's
pre-execute gate cannot host them today: it only reads
`accessed_resources(&self, params) -> Vec<ResourceAccess>`, which is
synchronous, params-only, and always satisfied by the approval cache.

1. **An async pre-execute hook with the tool context**, e.g.
   `async fn pre_execute_ask(&self, params, ctx) -> Option<PreExecuteAsk>`
   carrying the accesses, whether the cache may satisfy them, a description
   and a preview. `accessed_resources` becomes its simplest case. The risk
   judge needs the LLM, the resolved cwd, whether the route is sandboxed
   (`ctx.sandbox`) and `ctx.events`.
2. **Uncached asks in the executor's gate.** A cached sandboxed approval must
   never authorize the unsandboxed form, and skill env approval is
   per-invocation.
3. **At most one ask per call.** A Bash call can prompt twice today
   (escalation, then the judge); a resumed re-execution carries one
   `PreDecided` decision. Merge both into one ask, uncached if either part is.
4. **The tools stop asking at those points**, so a resumed re-execution under
   `PreDecided` never prompts again.
5. **Order kept**: a skill with missing env vars still errors before anyone
   is asked.

Afterwards only the post-failure retry stays during-execute, and the deadline
above is the only thing that governs it. One further option for that case:
an approval that outlives its future could re-dispatch straight to the
unsandboxed attempt, since the sandboxed one already failed.

## Open questions

- Is 30 minutes right for the deadline, and should the card say when it
  expires?
- After the move, should the post-failure retry be made resumable as above,
  or keep the deadline?
