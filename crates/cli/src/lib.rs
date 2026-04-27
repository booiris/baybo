//! Operator-facing command layer for Aura.
//!
//! See `docs/modules/cli.md` for the high-level design. This crate owns
//! the clap tree, the shared `CommandContext`, the dispatcher, and a
//! `SlashHandler` implementation that lets channel adapters intercept
//! `/`-prefixed input through the same code path as the shell CLI.
//!
//! ```no_run
//! use aura_cli::Cli;
//! use clap::Parser;
//!
//! let cli = Cli::parse();
//! // main.rs drives the rest after building a CommandContext.
//! ```

pub mod cli;
mod commands;
pub mod context;
pub mod dashboard;
pub mod dispatch;
pub mod error;
pub mod format;
pub mod slash;

pub use cli::{Cli, Commands, GlobalArgs};
#[cfg(any(test, feature = "test-support"))]
pub use commands::channel::run_registration;
pub use context::{CommandContext, ContextBuilder, Invocation};
pub use dashboard::CliDashboardProvider;
pub use dispatch::run;
pub use error::{CliError, Result};
pub use format::{CommandOutput, OutputFormat};
pub use slash::CliSlashHandler;

/// Emit a shell completion script without needing a `CommandContext`.
///
/// This is a shortcut for `Commands::Completion { shell }` so that `src/main.rs`
/// can handle the subcommand before booting the rest of the process.
pub fn completion_script(shell: cli::ShellKind) -> Result<CommandOutput> {
    commands::completion::handle(shell)
}
