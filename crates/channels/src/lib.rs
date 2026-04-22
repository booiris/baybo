mod channel;
mod error;
mod registry;
mod slash;
mod types;

pub mod registration;
pub mod wire;

pub use channel::Channel;
pub use error::ChannelError;
pub use registration::{
    Prompter, RegistrationFlow, RegistrationResult, TelegramRegistration,
    builtin_registration_flows,
};
pub use registry::ChannelRegistry;
pub use slash::{
    DashboardProvider, DashboardSnapshot, SlashCommand, SlashHandler, SlashOutcome, ViewKind,
};
pub use types::{AgentOutput, IncomingMessage, Message, NoticeLevel, OutgoingMessage};

pub type Result<T> = std::result::Result<T, ChannelError>;
