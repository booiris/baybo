use async_trait::async_trait;
use aura_model::ContentBlock;

/// Bare name (no leading `/`) of the compact slash command. Used by
/// the gateway slash manifest to register the command with sidecars
/// (Telegram `setMyCommands`, …); paired with [`COMPACT_COMMAND`] for
/// the form users actually type.
pub const COMPACT_COMMAND_NAME: &str = "compact";

/// Full leading-slash form of the compact command — what users type
/// and what the agent actor matches against to short-circuit into a
/// compression pass.
pub const COMPACT_COMMAND: &str = "/compact";

/// Bare name (no leading `/`) of the stop slash command. Published in the
/// gateway slash manifest + TUI command list for discoverability; paired
/// with [`STOP_COMMAND`] for the form users type.
pub const STOP_COMMAND_NAME: &str = "stop";

/// Full leading-slash form of the stop command. Recognised by the agent
/// `Router` (out-of-band of any running turn) to cancel the in-flight turn
/// and every in-flight subagent the session spawned. Like [`COMPACT_COMMAND`]
/// it is published but `PassThrough` at every edge — execution is central.
pub const STOP_COMMAND: &str = "/stop";

/// One-line description shared by every surface that lists `/stop`.
pub const STOP_COMMAND_DESCRIPTION: &str = "Stop the in-progress reply and any background tasks";

/// Bare name (no leading `/`) of the goal slash command. Published in the
/// gateway slash manifest + TUI command list for discoverability; paired
/// with [`GOAL_COMMAND`] for the form users type.
pub const GOAL_COMMAND_NAME: &str = "goal";

/// Full leading-slash form of the goal command. Recognised by the agent actor
/// to set / view / pause / resume / clear the session's autonomous objective
/// (see `docs/modules/goal.md`).
pub const GOAL_COMMAND: &str = "/goal";

/// One-line description shared by every surface that lists `/goal`.
pub const GOAL_COMMAND_DESCRIPTION: &str =
    "Set, view, or control an autonomous goal the agent self-drives to completion";

/// The `--budget` flag on `/goal <objective> --budget N` — caps the per-goal
/// token spend. Shared so the actor parser and any help text agree.
pub const GOAL_BUDGET_FLAG: &str = "--budget";

/// `/goal` sub-commands that control an existing goal (no objective argument).
pub const GOAL_PAUSE_SUBCOMMAND: &str = "pause";
pub const GOAL_RESUME_SUBCOMMAND: &str = "resume";
pub const GOAL_CLEAR_SUBCOMMAND: &str = "clear";

/// The acknowledgement line a `/stop` emits **only when it actually cancelled
/// an in-progress reply** (a no-op `/stop` after a turn already finished says
/// "Nothing in progress to stop." instead). The agent emits it in the stop
/// notice; the chat-history reconstruction keys off it to tell a
/// genuinely-cancelled turn (fold the partial into a "Cancelled" work block)
/// apart from a completed answer that merely had a `/stop` typed after it
/// (leave the answer intact). Shared so the producer and the matcher can't
/// drift.
pub const STOP_CANCELLED_REPLY_LINE: &str = "- Cancelled the in-progress reply.";

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
/// - [`SlashOutcome::NewSession`] — the handler asks the adapter to abandon
///   the current session (`/new`). Subscribed-kind adapters (TUI, web chat)
///   honour this by minting a fresh session id and switching their
///   subscription; adapters that have no session-switch concept should
///   treat it as `Handled(empty)`.
#[derive(Debug)]
pub enum SlashOutcome {
    Handled(Vec<ContentBlock>),
    OpenView(ViewKind),
    PassThrough,
    Exit,
    NewSession,
}

/// Built-in dashboard views that a channel adapter may render.
///
/// This is a closed enum rather than an opaque string so adapters can
/// exhaustively handle each view without depending on the command layer.
///
/// The set must stay a subset of the slash-command surface — every
/// view here corresponds to a top-level slash family that is also
/// admissible under `slash_admissible`. Adding a view that has no
/// slash counterpart bait operators with a TUI panel they can't
/// reach via the chat surface, and conversely removing a slash
/// family means dropping its view too (e.g. the former `Memory`
/// view was retired when `aura memory` was deleted from the CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewKind {
    Skills,
    Jobs,
    Sessions,
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
