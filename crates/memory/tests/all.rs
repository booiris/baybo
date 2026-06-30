// Aggregator: each sibling file is mounted as a module so the harness links
// once. `common` is declared here at the test-crate root and the backend
// suites reference it as `crate::common`. See the Cargo.toml `autotests =
// false` rationale.

#[path = "common/mod.rs"]
mod common;
#[path = "mem0.rs"]
mod mem0;
#[path = "openviking.rs"]
mod openviking;
