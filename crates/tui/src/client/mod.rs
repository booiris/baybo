//! Gateway client used by the TUI.
//!
//! The TUI talks exclusively to the gateway's channel UDS listener —
//! sessions, messages, approvals, and the SSE streams that carry model
//! output. Admin routes (status, config, jobs, …) are deliberately out
//! of reach; `aura cli` is the tool for those.
//!
//! DTOs are either borrowed from `aura-model` (entities) or defined
//! locally in [`dto`] (envelopes, request bodies). SSE shapes
//! ([`SseEvent`], [`ApprovalEvent`]) duplicate the gateway's serde
//! shapes rather than re-exporting them — pulling `aura-gateway` into
//! `aura-tui` would introduce a dependency cycle. The JSON wire format
//! pins them in sync; a mismatch surfaces immediately in the
//! round-trip integration tests.
//!
//! Transport: a hand-rolled hyper HTTP/1 client over
//! [`tokio::net::UnixStream`] (see [`uds::UdsTransport`]). SSE parsing
//! is a byte-level reader (see [`sse`]) so the client pulls in no
//! SSE-crate dependency.

pub mod dashboard;
pub mod dto;
pub mod http;
pub mod slash;
pub mod sse;
pub mod transport;
pub mod uds;

pub use dashboard::GatewayDashboardProvider;
pub use dto::{ApprovalEvent, ClientError, ClientResult, SseEvent};
pub use http::GatewayClient;
pub use slash::GatewaySlashHandler;
pub use sse::SseStream;
pub use transport::GatewayTransport;
