mod channel;
mod connection;
mod error;
mod kind;
mod registry;
mod slash;
mod types;

pub mod register_wire;
pub mod registration;

/// The wire types (`Frame`, `Message`, …) + MessagePack codec, now their own
/// crate so the iOS companion can speak the protocol without `baybo-channels`'
/// server-only dependency chain. Re-exported here as `wire` so existing
/// `baybo_channels::wire::*` consumers are unchanged.
pub use wire;

pub use channel::{ApprovalSurface, Channel, DispatchObserver, SubscribedView};
pub use connection::{Connection, ConnectionId, ConnectionSink, SendOutcome};
pub use error::{ChannelError, ConnectionNotFoundError};
pub use kind::ChannelKind;
pub use registration::{Prompter, RegistrationResult};
pub use registry::ChannelRegistry;
pub use slash::{
    COMPACT_COMMAND, COMPACT_COMMAND_NAME, DashboardProvider, DashboardSnapshot,
    STOP_CANCELLED_REPLY_LINE, STOP_COMMAND, STOP_COMMAND_DESCRIPTION, STOP_COMMAND_NAME,
    SlashCommand, SlashHandler, SlashOutcome, ViewKind,
};
pub use types::{
    AgentEvent, AgentOutput, IncomingMessage, Message, NoticeLevel, OutgoingMessage, RouterInbound,
    SessionEvent, StatusPhase, ToolStatus,
};
// `MessageRole` now lives in `wire`; keep it at the crate root so
// `baybo_channels::MessageRole` consumers are unchanged.
pub use wire::MessageRole;

pub type Result<T> = std::result::Result<T, ChannelError>;
