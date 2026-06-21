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
    AgentEvent, AgentOutput, IncomingMessage, Message, MessageRole, NoticeLevel, OutgoingMessage,
    RouterInbound, SessionEvent, ToolStatus, TurnStatus,
};

pub type Result<T> = std::result::Result<T, ChannelError>;
