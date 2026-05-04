# Trace Redesign — done, follow-ups remaining

The full design is implemented and reflected in:

- [`docs/modules/session.md`](../modules/session.md) — Session, lineage, trigger, fork rejection
- [`docs/modules/job.md`](../modules/job.md) — Job state machine (`Completed`, `Cancelled`, etc.)
- [`docs/modules/trace.md`](../modules/trace.md) — Step / Span / SpanEvent

These specs are authoritative; this file no longer carries the active design.

## Follow-ups

Tracked here so they don't get lost. Each is a scoped extension, not a redesign.

### TUI live trace stream

`TraceEventStream` flows in-process. Surfacing it across the gateway WS
protocol to the TUI for live progress display requires a new frame
variant (`Frame::TraceEvent` or similar) plus a TUI render layer. Scoped
to whichever PR adds the live-progress view.

### CostSubscriber per-model pricing accuracy

`LlmProviderFactory::known_pricings()` (added so `CostSubscriber` can
attribute spend to non-active models) currently reports one flat rate
per provider — every model in `OpenAIProviderFactory::known_models()`
returns `2.50 / 10.0`, every Anthropic model returns `3.0 / 15.0`,
etc. This mirrors what `create()` always did; the goal of `known_pricings`
itself was just "non-active models attribute at *some* non-zero rate"
rather than "exactly correct per-model rate."

The next step is per-model accurate pricing — `gpt-5-mini` should
not attribute at `gpt-5`'s rate. Two viable shapes:

- A static `HashMap<&'static str, ModelPricing>` per provider, replacing
  the flat `2.50 / 10.0` block in `create()` and the flat block in
  `known_pricings()`. Cheap and self-documenting; the cost subscriber
  benefits automatically.
- Pull from a provider catalog endpoint at boot (extending
  `LiveModelInfo` with a pricing field where the provider exposes
  one). More accurate but credential- / network-bound.

The `CostSubscriberMetrics.lagged_events` counter (also added in this
PR) is the canary for "we're undercounting" — when per-model pricing
lands, the same metric will track it.

### Rewrite `cli/tests/dispatch_smoke.rs`

The original `dispatch_smoke.rs` was deleted in this PR — its 2k lines
of hand-rolled `MemorySessionStore` / `MemoryJobStore` / `TraceMemStore`
mocks were locked to the pre-redesign trace types (`SessionTrace`,
`TraceNode`, `TraceFork`) that no longer exist, and the canonical
fakes in `aura_storage::test_support` cover the same surface now.
Two named tests it carried were referenced from
`docs/todo/cli-agent-send-argv.md`:

- `agent_send_slash_mode_is_forbidden` — asserts the
  `AgentSendForbiddenInSlash` guard (`crates/cli/src/commands/agent.rs:28-32`)
  stays in place. Pin this against the canonical fakes when `agent send`
  argv-mode wiring lands.
- `agent_send_argv_mode_reports_deferred` — asserts the partial-ship
  error string today, will flip to assert a successful reply when the
  real wiring lands.

Until `agent send` argv-mode is implemented, the slash-mode guard is
covered indirectly via `crates/cli/src/commands/agent.rs` unit-level
tests. Restore the two named tests as part of the argv-mode PR.
