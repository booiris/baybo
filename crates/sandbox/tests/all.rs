// Aggregator: each sibling file is mounted as a module so the harness links
// once. Each file keeps its own `#![cfg(...)]` gate (linux / macos / docker)
// as a module-inner attribute. See the Cargo.toml `autotests = false` rationale.

#[path = "bwrap_smoke.rs"]
mod bwrap_smoke;
#[path = "docker_smoke.rs"]
mod docker_smoke;
#[path = "sandbox_exec_smoke.rs"]
mod sandbox_exec_smoke;
