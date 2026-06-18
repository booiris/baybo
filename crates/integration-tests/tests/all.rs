// Aggregator: each sibling file is mounted as a module so the harness
// links once. See the Cargo.toml `autotests = false` rationale.

#[path = "agent_loop_e2e.rs"]
mod agent_loop_e2e;
#[path = "background_compression_e2e.rs"]
mod background_compression_e2e;
#[path = "channel_registration.rs"]
mod channel_registration;
#[path = "context_compression_e2e.rs"]
mod context_compression_e2e;
#[path = "goal_continuation_e2e.rs"]
mod goal_continuation_e2e;
#[path = "security_pipeline.rs"]
mod security_pipeline;
#[path = "smoke.rs"]
mod smoke;
#[path = "streaming_safety.rs"]
mod streaming_safety;
#[path = "summary_aware_wrapper_e2e.rs"]
mod summary_aware_wrapper_e2e;
#[path = "token_calibration_e2e.rs"]
mod token_calibration_e2e;
#[path = "tool_boundary.rs"]
mod tool_boundary;
#[path = "tool_concurrency.rs"]
mod tool_concurrency;
