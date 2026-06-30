// Aggregator: each sibling file is mounted as a module so the harness links
// once. See the Cargo.toml `autotests = false` rationale.

#[path = "config.rs"]
mod config;
#[path = "mirror_contract.rs"]
mod mirror_contract;
