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
/// - [`SlashOutcome::PassThrough`] — the input was not a recognised command;
///   the adapter should treat it as an ordinary user message.
/// - [`SlashOutcome::Exit`] — the handler requests that the adapter stop.
#[derive(Debug)]
pub enum SlashOutcome {
    Handled(Vec<ContentBlock>),
    PassThrough,
    Exit,
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
}
