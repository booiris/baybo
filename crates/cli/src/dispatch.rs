use crate::cli::Commands;
use crate::commands;
use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

/// Route a parsed command to its handler.
pub async fn run(ctx: &CommandContext, cmd: Commands) -> Result<CommandOutput> {
    match cmd {
        Commands::Config { cmd } => commands::config::handle(ctx, cmd).await,
        Commands::Skills { cmd } => commands::skills::handle(ctx, cmd),
        Commands::Tools { cmd } => commands::tools::handle(ctx, cmd).await,
        Commands::Channels { cmd } => commands::channels::handle(ctx, cmd).await,
        Commands::Llm { cmd } => commands::llm::handle(ctx, cmd),
        Commands::Workspace { cmd } => commands::workspace::handle(ctx, cmd).await,
        Commands::Session { cmd } => commands::session::handle(ctx, cmd).await,
        Commands::Job { cmd } => commands::job::handle(ctx, cmd).await,
        Commands::Cron { cmd } => commands::cron::handle(ctx, cmd).await,
        Commands::Memory { cmd } => commands::memory::handle(ctx, cmd).await,
        Commands::Trace { cmd } => commands::trace::handle(ctx, cmd).await,
        Commands::Status => commands::status::handle(ctx).await,
        Commands::Doctor => commands::doctor::handle(ctx).await,
        Commands::Completion { shell } => commands::completion::handle(shell),
    }
}
