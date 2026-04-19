//! HTTP + SSE client for talking to an `aura gateway` from the TUI.
//!
//! The TUI is the only intended consumer, so the client lives inside
//! the TUI crate rather than as a reusable crate. Methods mirror the
//! gateway's v1 surface one-to-one; DTOs are either borrowed from
//! `aura-model` (entities) or defined locally in [`dto`] (envelopes,
//! request bodies).
//!
//! SSE shapes ([`SseEvent`], [`ApprovalEvent`]) duplicate the gateway's
//! serde shapes rather than re-exporting them — pulling `aura-gateway`
//! into `aura-tui` would introduce a dependency cycle. The types
//! are small and the JSON wire format pins them in sync; a mismatch
//! would surface immediately in the round-trip integration tests.
//!
//! Transport choices:
//! * HTTP via `reqwest` with `json` + `stream` features.
//! * SSE parsing is a hand-rolled byte-level reader (see [`sse`]) so
//!   the client has no SSE-crate dependency.

pub mod dashboard;
pub mod dto;
pub mod http;
pub mod slash;
pub mod sse;
pub mod transport;

pub use dashboard::GatewayDashboardProvider;
pub use dto::{ApprovalEvent, ClientError, ClientResult, SseEvent};
pub use http::GatewayClient;
pub use slash::GatewaySlashHandler;
pub use sse::SseStream;
pub use transport::GatewayTransport;
