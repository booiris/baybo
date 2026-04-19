//! HTTP route modules for the gateway.
//!
//! Split along the two-listener contract:
//!
//! * [`admin`] — TCP, bearer-token authenticated. No chat content.
//! * [`channel`] — UDS, peer-credential + PSK/token authenticated.
//!   Session CRUD, messages, approvals, SSE streams.
//! * [`health`] — `/healthz` + `/readyz`, served by both listeners with
//!   no auth.
//! * [`webui`] — embedded React dashboard. Admin-listener fallback.
//! * [`dto`] — shared request/response envelopes.

pub mod admin;
pub mod channel;
pub mod dto;
pub mod health;
pub mod webui;
