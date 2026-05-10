//! End-to-end test fixtures for the aura workspace.
//!
//! This crate exposes builders and helpers that wire multiple
//! `test-support`-feature crates together so individual `tests/*.rs`
//! files in this crate can stay focused on the scenario under test.
//!
//! Conventions:
//! - Domain types get a `<Type>Builder` so tests read top-down.
//! - Spies (`RecordingChannel`, `RecordingTool`) over mocks — record then
//!   assert.
//! - `capture_tracing()` wires a per-test in-memory subscriber so log
//!   assertions don't depend on global state.

pub mod fixtures;
pub mod harness;
pub mod tracing_capture;

pub use fixtures::{SessionBuilder, gateway_with_memory_vault, master_key_for_tests};
pub use harness::{AgentTestHarness, AgentTestHarnessBuilder, CompressionStrategyFactory};
pub use tracing_capture::{CapturedEvent, TracingCapture, capture_tracing};
