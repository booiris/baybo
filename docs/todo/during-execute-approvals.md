# During-execute approvals

> **Status:** shipped. All four approval prompts are now resumable across
> restarts and idle releases: the three pre-work prompts moved to
> before-execute (`Tool::pre_execute_ask`), and Bash's post-failure unsandboxed
> retry opts in to resumable during-execute continuation via
> `Tool::during_execute_resumer` + `DuringExecuteResumer`
> (`AskPhase::DuringExecuteResumable`). Plain `AskPhase::DuringExecuteNonResumable`
> (default for any tool that does not implement `DuringExecuteResumer`, or any
> prompt raised via `ApprovalHandle::request_uncached`) stays
> non-resumable and keeps `DURING_EXECUTE_APPROVAL_TIMEOUT` (30 min) and
> `ActorStopCost::AbandonsAsks`.

## The phases

An approval is raised in one of three phases (`AskPhase`):

- **Before execute (`AskPhase::BeforeExecute`)** — the executor's pre-execute
  gate (`Tool::pre_execute_ask`), before the tool runs. Resumable: re-dispatches
  the persisted `tool_use` through `Tool::execute` with
  `PreExecuteGate::PreDecided { decision, phase: BeforeExecute, accesses }`.
- **During execute, resumable (`AskPhase::DuringExecuteResumable`)** — raised
  through `ApprovalHandle::request_resumable_uncached(self, ...)` by a tool
  that implements `DuringExecuteResumer` and returns `Some(self)` from
  `Tool::during_execute_resumer()` (today: `BashTool` for its post-failure
  unsandboxed retry). Resumable with no deadline: an approval after a restart
  or idle release skips `pre_execute_ask` (uncached) and invokes
  `DuringExecuteResumer::resume_during_execute`. A dead denial/abandon resolves
  to the standard `ToolError::Denied`.
- **During execute, non-resumable (`AskPhase::DuringExecuteNonResumable`)** — the fail-safe
  default for any tool that raises a mid-execution prompt via
  `ApprovalHandle::request_uncached`. Governed by
  `DURING_EXECUTE_APPROVAL_TIMEOUT` (30 min), guarded from idle-reaper
  abandonment by `ActorStopCost::AbandonsAsks`, and closed/abandoned if its
  tool future dies.

## Where each approval runs

| Prompt | Where | Phase | On Resume (`Approve` / `ApproveAlways`) |
|---|---|---|---|
| Bash `require_escalated` | `BashTool::pre_execute_ask` (uncached, merges destructive warning) | `BeforeExecute` | `Tool::execute` (uncached) |
| Bash risk judge (`permission = auto`) | `BashTool::pre_execute_ask` (cached; skipped on `PreDecided`) | `BeforeExecute` | `Tool::execute` (cached if `ApproveAlways`, skips judge) |
| Skill env vars | `SkillTool::pre_execute_ask` (uncached; after host-env and risk-block checks) | `BeforeExecute` | `Tool::execute` (uncached) |
| Bash retry after a sandbox failure | `request_unsandboxed_retry_approval` | `DuringExecuteResumable` | `BashTool::resume_during_execute` (uncached, runs unsandboxed directly with `sandbox_escalation`) |

`permission = manual` Bash declares its commands through `accessed_resources` /
`pre_execute_ask`, so those prompts are `BeforeExecute` as well.

## Summary of what shipped

1. **Async pre-execute hook with the tool context**:
   `Tool::pre_execute_ask(&self, params, ctx, check: PreExecuteCheck) -> Option<PreExecuteAsk>`
   carries `accesses`, `cacheable`, `description`, and `params_preview`. Its
   default delegates to `accessed_resources` + `progress_label` (`cacheable: true`).
   `ToolExecutor` constructs `ToolContext` ahead of the gate and runs
   `pre_execute_ask` on the same revealed params as execution. Returned approval
   data is re-masked before use; the original ledger resources bound resumed
   approvals, and Skill checks its current env requirements against the per-call
   grant. The executor flushes any `ctx.events` (such as
   risk-judge `LlmCall`/`Phase` events) before the gate returns.
2. **Uncached asks in the executor's gate**: when `PreExecuteAsk::cacheable` is
   `false`, the executor neither consults `approved_resources` before prompting
   nor persists `ApproveAlways` into `approved_resources` (live or `PreDecided`).
3. **At most one ask per call**: `BashTool::pre_execute_ask` produces a single
   `PreExecuteAsk`, merging the destructive-command warning into an uncached
   `require_escalated` prompt rather than prompting twice.
4. **Tools stop asking at those three points**: `BashTool::execute` and
   `SkillTool::execute` no longer prompt for escalation, pre-exec risk judge,
   or skill env vars, and `PreExecuteCheck::PreDecided` lets resumed calls skip
   redundant LLM/risk checks without re-prompting.
5. **Order kept**: a skill with missing host env vars (or blocked by the risk
   assessor) returns `None` from `pre_execute_ask` and still errors in
   `execute()` before anyone is asked.
6. **Generic tool-owned during-execute resume**: `Tool` exposes a single
   capability method `during_execute_resumer(&self) -> Option<&dyn DuringExecuteResumer>`
   (default `None`), binding declaration and continuation implementation into a
   single source of truth at compile time. Tools that can resume a mid-call
   prompt call `ApprovalHandle::request_resumable_uncached(self, ...)` (which
   requires `&dyn DuringExecuteResumer` at compile time and verifies
   `Tool::during_execute_resumer().is_some()` at runtime). `PreExecuteGate::PreDecided { decision, phase, accesses }`
   dispatches `DuringExecuteResumer::resume_during_execute` without leaking any
   Bash/sandbox concept into the generic gate or `ToolContext`. Ordinary
   `request_uncached` prompts keep `AskPhase::DuringExecuteNonResumable` with
   `DURING_EXECUTE_APPROVAL_TIMEOUT` and `ActorStopCost::AbandonsAsks`.
