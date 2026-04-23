//! HTTP route modules for the gateway.
//!
//! Split along the two-listener contract:
//!
//! * [`admin`] — TCP, bearer-token authenticated. No chat content.
//! * [`health`] — `/healthz` + `/readyz`, served by both listeners
//!   with no auth. (The channel TCP listener's chat surface is the
//!   WS endpoint in [`crate::channel`] — not a REST router.)
//! * [`webui`] — embedded React dashboard. Admin-listener fallback.
//! * [`dto`] — shared request/response envelopes.

pub mod admin;
pub mod dto;
pub mod health;
pub mod openapi;
pub mod webui;
