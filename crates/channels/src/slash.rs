use async_trait::async_trait;
use aura_model::ContentBlock;

/// Outcome of inspecting a `/`-prefixed line.
///
/// Channel adapters call [`SlashHandler::handle`] on any input that starts
/// with `/` (excluding adapter-local reserved tokens such as `/quit`) and
/// react to the result:
///
/// - [`SlashOutcome::Handled`] — the handler consumed the input and produced
///   response blocks that should be written back to the user. The adapter
///   must not forward the original line to the agent.
/// - [`SlashOutcome::OpenView`] — the handler recognised a bare dashboard
///   command (e.g. `/skills`) and wants the adapter to switch to a dashboard
///   view. Only adapters that support interactive views (TUI) honour this;
///   others should treat it as `Handled(empty)`.
/// - [`SlashOutcome::PassThrough`] — the input was not a recognised command;
///   the adapter should treat it as an ordinary user message.
/// - [`SlashOutcome::Exit`] — the handler requests that the adapter stop.
#[derive(Debug)]
pub enum SlashOutcome {
    Handled(Vec<ContentBlock>),
    OpenView(ViewKind),
    PassThrough,
    Exit,
}

/// Built-in dashboard views that a channel adapter may render.
///
/// This is a closed enum rather than an opaque string so adapters can
/// exhaustively handle each view without depending on the command layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewKind {
    Skills,
    Jobs,
    Sessions,
    Memory,
}

/// Snapshot of tabular data fed to a dashboard view.
///
/// Kept as a flat value type (no trait objects, no `serde`) — purely a render
/// input produced on demand by a [`DashboardProvider`] implementor.
#[derive(Debug, Clone)]
pub struct DashboardSnapshot {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub footer: Option<String>,
}

/// Source of dashboard data for the TUI.
///
/// Implementations live outside this crate (e.g. in `aura-cli`) and carry
/// the necessary manager `Arc`s. Defining only the trait here keeps the
/// `channels` crate free of upstream business dependencies.
#[async_trait]
pub trait DashboardProvider: Send + Sync {
    /// Build a fresh snapshot for the requested view.
    async fn snapshot(&self, kind: ViewKind) -> DashboardSnapshot;
}

/// Pluggable interceptor for in-conversation slash commands.
///
/// Implementations live outside this crate (e.g. `aura-cli`). Defining the
/// trait here keeps channel adapters independent of any specific command
/// layer while letting every adapter hook into the same dispatcher.
#[async_trait]
pub trait SlashHandler: Send + Sync {
    /// Inspect a raw line (including the leading `/`) and decide how the
    /// adapter should react.
    async fn handle(&self, raw: &str) -> SlashOutcome;

    /// List completion candidates the adapter may surface to the user.
    ///
    /// Adapters call this once at startup and cache the result. The list is
    /// considered static for the session; changing command shapes at runtime
    /// is out of scope.
    fn commands(&self) -> Vec<SlashCommand> {
        Vec::new()
    }
}

/// One completion candidate for a slash command.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// The leading-slash form shown to the user (e.g. `/skills`).
    pub name: String,
    /// One-line hint displayed next to the candidate in the completion popup.
    pub description: String,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}
