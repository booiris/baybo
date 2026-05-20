use crate::cli::Commands;
use crate::commands;
use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

/// Route a parsed command to its handler.
pub async fn run(ctx: &CommandContext, cmd: Commands) -> Result<CommandOutput> {
    match cmd {
        Commands::Config { cmd } => commands::config::handle(ctx, cmd).await,
        Commands::Skills { cmd } => commands::skills::handle(ctx, cmd).await,
        Commands::Agents { cmd } => commands::agents::handle(ctx, cmd).await,
        Commands::Channel { cmd } => commands::channel::handle(ctx, cmd).await,
        Commands::Mcp { cmd } => commands::mcp::handle(ctx, cmd).await,
        Commands::Pair { cmd } => commands::pair::handle(ctx, cmd).await,
        Commands::Llm { cmd } => commands::llm::handle(ctx, cmd).await,
        Commands::ExternalAgent { cmd } => commands::external_agent::handle(ctx, cmd).await,
        Commands::Session { cmd } => commands::session::handle(ctx, cmd).await,
        Commands::Job { cmd } => commands::job::handle(ctx, cmd).await,
        Commands::Cron { cmd } => commands::cron::handle(ctx, cmd).await,
        Commands::Log { cmd } => commands::log::handle(ctx, cmd).await,
        Commands::Agent { cmd } => commands::agent::handle(ctx, cmd).await,
        Commands::Cost { cmd } => commands::cost::handle(ctx, cmd).await,
        Commands::Status { live } => commands::status::handle(ctx, live).await,
        Commands::Doctor => commands::doctor::handle(ctx).await,
        Commands::Completion { shell } => commands::completion::handle(shell),
        Commands::Tui { .. } => Err(crate::error::CliError::UnknownCommand(
            "`tui` is an interactive session; main.rs handles it before dispatch".into(),
        )),
        Commands::Gateway { .. } => Err(crate::error::CliError::UnknownCommand(
            "`gateway` runs a long-lived server; main.rs handles it before dispatch".into(),
        )),
        Commands::Setup => Err(crate::error::CliError::AgentSendForbiddenInSlash(
            "setup".into(),
        )),
    }
}
