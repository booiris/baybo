// Aggregator: see Cargo.toml `autotests = false` rationale.

// `dispatch_smoke` is disabled until its hand-rolled MemorySessionStore /
// MemoryJobStore / TraceMemStore mocks are migrated to the new
// `aura-storage` traits (the SessionTrace/TraceNode types those mocks
// implemented were removed by the trace redesign — see
// `docs/todo/trace-redesign.md`). Tracked by step-5 follow-up; the
// canonical fakes in `aura_storage::test_support` should replace the
// local mocks.
// #[path = "dispatch_smoke.rs"]
// mod dispatch_smoke;
#[path = "mcp_e2e.rs"]
mod mcp_e2e;
#[path = "parser.rs"]
mod parser;
