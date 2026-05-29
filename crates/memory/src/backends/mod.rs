//! Concrete [`crate::Memory`] backends.
//!
//! Each submodule owns one provider: its config struct, HTTP client,
//! `Memory` impl, and `Tool` impls. The trait surface, error type, trace
//! context, and the [`crate::boot`] dispatcher are deliberately kept out
//! of this folder — `backends/` is for plug-ins, not core wiring.

pub mod mem0;
pub mod openviking;
