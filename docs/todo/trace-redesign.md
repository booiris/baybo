# Trace Redesign — done, follow-ups remaining

The full design is implemented and reflected in:

- [`docs/modules/session.md`](../modules/session.md) — Session, lineage, trigger, fork rejection
- [`docs/modules/job.md`](../modules/job.md) — Job state machine (`Completed`, `Cancelled`, etc.)
- [`docs/modules/trace.md`](../modules/trace.md) — Step / Span / SpanEvent

These specs are authoritative; this file no longer carries the active design.

## Follow-ups

Tracked here so they don't get lost. Each is a scoped extension, not a redesign.

### CostManager cache-tier pricing

Per-model input/output rates are now sourced from a bundled OpenRouter
snapshot (`crates/llm/src/providers/openrouter_pricings.json`,
refreshed via `scripts/regen-openrouter-pricings.sh`).
`crate::openrouter::slug_for(provider, model_id)` algorithmically maps
Aura model ids to OpenRouter slugs (no per-provider hand tables);
`LlmProviderFactory::pricing_for_model` is the trait entry that
combines the snapshot lookup with each factory's
`flat_default_pricing()`. At boot, `runtime.rs` spawns a task that
calls `aura_llm::openrouter::fetch_overlay_for(&entries)` for every
configured entry then `CostManager::merge_pricings`, looping on
`openrouter::REFRESH_INTERVAL` (24h) with a
`tokio::select!` against the shared `ShutdownSignal`. Failures
silently keep the snapshot value; drift ≥`PRICING_DRIFT_WARN` (25%)
warns.

Still open: the snapshot already carries
`input_cache_read` / `input_cache_write` rates per model, but
`ModelPricing` exposes only `input_per_1m_tokens` and
`output_per_1m_tokens`. Anthropic-cache-heavy workflows therefore
under-attribute on cache writes (1.25× input) and over-attribute on
cache reads (~10× the real rate).

To close this: widen `ModelPricing` with optional
`cached_input_per_1m_tokens` / `cache_write_per_1m_tokens` fields,
plumb `TokenUsage::cached_input_tokens` and
`cache_creation_input_tokens` (already populated end-to-end) through
`agent::cost::compute_cost_usd`, and surface non-zero values in
`fetch_overlay_for`. The plumbing crosses `aura-llm` (data shape) →
`aura-agent` (compute_cost_usd signature) → `cost_records` schema
(new columns or a JSON bag), which is why it sits behind the
input/output PR rather than landing with it.

Codex adversarial-review (2026-05-09) flagged a related fail-open
risk: `merge_pricings` can lower or zero out the bundled rate if
OpenRouter ever returns 0 for a paid model (placeholder, transition,
upstream bug). Today only `warn!` fires on drift; the merge proceeds
unconditionally. Conservative fix: take `max(prev, live)` per field
in the overlay so a live downward drift can never widen the budget,
or reject `live <= 0` outright. Worth folding into the cache-tier
widening PR since both touch `merge_pricings`.

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
