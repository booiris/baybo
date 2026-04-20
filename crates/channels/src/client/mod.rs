//! Plugin-side SDK for talking to the gateway's channel UDS listener.
//!
//! Gated behind the `client` Cargo feature so the agent/gateway/cli
//! consumers that only need the in-process [`ChannelAdapter`] trait +
//! shared [`wire`] types don't pull in the HTTP-client dependency chain.
//!
//! Surface area:
//!
//! - [`GatewayClient`] — DTO-shaped facade over the channel listener's
//!   HTTP+SSE surface.
//! - [`UdsTransport`] — lower-level `hyper` HTTP/1 client over
//!   [`tokio::net::UnixStream`]; exposed for plugins that need to reach
//!   routes the client hasn't wrapped yet.
//! - [`SseStream`] — a typed wrapper around the gateway's Server-Sent
//!   Event streams, driven by the WHATWG SSE grammar.
//! - [`dto`] — request/response bodies for individual endpoints.
//!
//! [`wire`]: crate::wire
//! [`ChannelAdapter`]: crate::ChannelAdapter

pub mod dto;
pub mod http;
pub mod sse;
pub mod uds;

pub use dto::{
    ClientError, ClientResult, CreateCronRequest, CreateSessionRequest, JobSummary, ListResponse,
    MemorySummary, MutateConfigResponse, ResolveApprovalRequest, ResolveApprovalResponse,
    SendMessageRequest, SendMessageResponse, SetConfigRequest, StatusResponse, StoreMemoryRequest,
    UnsetConfigRequest,
};
pub use http::GatewayClient;
pub use sse::SseStream;
pub use uds::UdsTransport;
