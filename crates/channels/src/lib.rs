mod channel;
mod connection;
mod error;
mod kind;
mod registry;
mod slash;
mod types;

pub mod register_wire;
pub mod registration;
pub mod wire;

pub use channel::{ApprovalSurface, Channel, DispatchObserver, MultiplexedView, SubscribedView};
pub use connection::{Connection, ConnectionId, ConnectionSink, SendOutcome};
pub use error::ChannelError;
pub use kind::ChannelKind;
pub use registration::{Prompter, RegistrationResult};
pub use registry::ChannelRegistry;
pub use slash::{
    COMPACT_COMMAND, COMPACT_COMMAND_NAME, DashboardProvider, DashboardSnapshot, SlashCommand,
    SlashHandler, SlashOutcome, ViewKind,
};
pub use types::{
    AgentOutput, IncomingMessage, Message, MessageRole, NoticeLevel, OutgoingMessage, SessionEvent,
};

pub type Result<T> = std::result::Result<T, ChannelError>;
