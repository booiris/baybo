mod channel;
mod error;
mod registry;
mod slash;
mod types;

pub mod register_wire;
pub mod registration;
pub mod wire;

pub use channel::Channel;
pub use error::ChannelError;
pub use registration::{Prompter, RegistrationResult};
pub use registry::ChannelRegistry;
pub use slash::{
    DashboardProvider, DashboardSnapshot, SlashCommand, SlashHandler, SlashOutcome, ViewKind,
};
pub use types::{AgentOutput, IncomingMessage, Message, NoticeLevel, OutgoingMessage};

pub type Result<T> = std::result::Result<T, ChannelError>;
