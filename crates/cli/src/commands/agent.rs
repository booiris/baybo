use serde_json::json;

use crate::cli::AgentCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: AgentCmd) -> Result<CommandOutput> {
    match cmd {
        AgentCmd::Send {
            session,
            message,
            yes,
        } => send(ctx, &session, &message, yes).await,
    }
}

async fn send(
    ctx: &CommandContext,
    session: &str,
    message: &str,
    _yes: bool,
) -> Result<CommandOutput> {
    // The slash guard fires first, regardless of `--yes`. The whole point of
    // `AgentSendForbiddenInSlash` is that you cannot drive an agent turn from
    // inside an agent turn — the active session is already consuming the
    // LLM/tool budget, and a nested turn would invert the supervisor model.
    if ctx.invocation == Invocation::Slash {
        return Err(CliError::AgentSendForbiddenInSlash(
            "agent send".to_string(),
        ));
    }

    // Argv path requires a synchronous one-shot entry on `Router` (or a
    // running daemon exposing an RPC surface). Neither exists yet; the CLI
    // grammar is shipped in advance so `aura agent send --help` documents
    // the operator-facing contract.
    Err(CliError::Manager(format!(
        "agent send is not yet available in argv mode (session={session}, \
         message.len={len}): the agent runtime does not yet expose a \
         synchronous one-shot entry point. Interactive use: launch `aura` \
         and type directly at the chat prompt.",
        len = message.len(),
    )))
}

/// Unused today, but kept so future one-shot wiring has a clear place to
/// produce structured output without touching the error path above.
#[allow(dead_code)]
fn build_ok(session: &str, reply: &str) -> CommandOutput {
    CommandOutput {
        human: format!("session {session}\n{reply}"),
        data: Some(json!({
            "session": session,
            "reply": reply,
        })),
    }
}
