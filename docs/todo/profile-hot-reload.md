# Profile Hot Reload

## Problem

`aura-agent::Soul` reads `profile/{SOUL,USER,IDENTITY}.md` once at `AgentLoop` construction and bakes them into a single `system_prompt` `String`. Any subsequent edit to those files — whether by a human, by the agent via `Edit` against a path under `profile/`, or a future skill — is invisible to the in-flight session: the cached prompt continues to drive every iteration.

This is intentional today: mid-session identity swaps would split the conversation between two personas and create a self-modification feedback loop where the agent could rewrite itself and immediately act on the new self. But it does mean a user who asks the agent to "add a constraint to your soul" has to wait for the next session before the agent picks up the change, which is an avoidable latency once we have safe primitives for the swap.

## Proposed Direction

Two separable workstreams; the first is cheap, the second is the hard one.

1. **Lazy reload at iteration boundary** (cheap, additive)
   - Have `Soul` hold an `Arc<aura_workspace::WorkspaceManager>` instead of a frozen `String`.
   - `Soul::system_prompt(&self) -> &str` becomes `async fn system_prompt(&self) -> Cow<'_, str>` backed by an `ArcSwap<String>` cache.
   - At the start of each `AgentLoop::run_iteration`, call `soul.refresh_if_changed()` which compares mtimes (or content hashes) of the three identity files to the cached prompt and rebuilds when stale.
   - The `Edit` profile-write path already lands writes synchronously, so no extra coordination is needed: the next iteration just observes the new mtime.

2. **Conversation-history coherence** (the actually-hard part)
   - Once the system prompt can change mid-session, the LLM has already been responding under the *old* prompt. The `ContextManager`-tracked history is consistent with the *old* identity.
   - Decide between (a) keep history as-is and let the new prompt only affect new turns — risks subtle drift; (b) rewrite history to drop now-inconsistent assistant turns — heavyweight; (c) start a fresh conversation thread under the new identity but keep memory — needs cooperation from `aura-context`.
   - Whatever we pick must guard against the self-modification loop: either rate-limit identity changes per session, or surface a Notice that flags "identity changed mid-session, proceed with caution," or both.

The first workstream can land independently behind a config flag (default off) so we get a way to dogfood lazy reload without committing to the conversation-coherence story yet.

## Related

- `crates/agent/src/soul.rs` — the cached `system_prompt` we need to make swappable
- `crates/tools/src/builtin/edit.rs` — the writer side; appends "change takes effect on the next session" to the tool output when the edit lands under `profile/`, and should drop that caveat once this work ships
- `docs/modules/agent.md` — where the user-facing description of identity-file behaviour lives
- `docs/todo/config-hot-reload.md` — same shape of problem on `aura-config`; some of the `ArcSwap` plumbing should be reusable
