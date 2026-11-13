# During-execute approvals do not survive their tool future

> **Status:** open, to discuss. Shipped behaviour is below; nothing here is
> decided.

## What happens today

An approval is raised in one of two phases (`AskPhase`):

- **Before execute** — the executor's pre-execute gate, before the tool runs.
  If the tool future goes away, the persisted `tool_use` can be dispatched
  again with the decision already made, so the ask is **resumable**.
- **During execute** — a tool that is already running asks through
  `ApprovalHandle` (a shell command that wants a new host, a file outside the
  approved tree). The command's partial state lives only in that future.
  `resumable()` (`crates/model/src/human_ask.rs`) says **not resumable**.

Whenever the tool future of a during-execute approval goes away, the ask is
abandoned: its card disappears and the call's result is the abandoned refusal.
That happens on:

- a gateway restart, which drops every parked future;
- an actor panic;
- the idle reaper, which never evicts such a session. Ending the turn would
  abandon the prompt, so the actor stays in memory until someone answers or
  `/stop`s. Sessions parked only on resumable asks are evicted after
  `idle_timeout` and resume through the Resume door
  (`AgentSupervisor::release_parked_turn`).

So the reaper's memory bound is: every session parked on a during-execute
approval keeps its actor, transcript and tool future in memory with no upper
limit on how long.

## Questions to settle

- Should a during-execute approval ever outlive its future? Re-running the
  whole call with the access pre-granted repeats whatever the command already
  did before it asked, so it is only safe for idempotent tools.
- If not, is an unbounded warm actor acceptable, or does a during-execute
  approval need a deadline after all (with the card saying so)?
- Could a tool declare its accesses up front more often, turning
  during-execute asks into before-execute ones?
