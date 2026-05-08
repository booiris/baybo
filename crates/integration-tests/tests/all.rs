// Aggregator: each sibling file is mounted as a module so the harness
// links once. See the Cargo.toml `autotests = false` rationale.

#[path = "agent_loop_e2e.rs"]
mod agent_loop_e2e;
#[path = "channel_registration.rs"]
mod channel_registration;
#[path = "context_compression_e2e.rs"]
mod context_compression_e2e;
#[path = "multimodal_tool_output.rs"]
mod multimodal_tool_output;
#[path = "security_pipeline.rs"]
mod security_pipeline;
#[path = "smoke.rs"]
mod smoke;
#[path = "streaming_safety.rs"]
mod streaming_safety;
#[path = "token_calibration_e2e.rs"]
mod token_calibration_e2e;
#[path = "tool_boundary.rs"]
mod tool_boundary;
