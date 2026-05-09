//! `aura agent send` argv vs slash dispatch contract.
//!
//! Two invariants this file pins, both originally covered by the
//! deleted `dispatch_smoke.rs`:
//!
//! 1. From inside a chat session (`Invocation::Slash`) the dispatcher
//!    rejects the command with [`CliError::AgentSendForbiddenInSlash`]
//!    BEFORE doing any work — driving an agent turn from inside an
//!    agent turn would invert the supervisor model and double-charge
//!    cost / tool budgets.
//! 2. From the shell (`Invocation::Argv`) the command is *grammatically
//!    accepted* but the runtime entry point is still deferred — the
//!    dispatcher returns [`CliError::Manager`] with the documented
//!    "not yet available in argv mode" string. Pinning the deferred
//!    error string makes a partial-ship of the argv path obvious in
//!    review (the assertion will need to flip when
//!    `docs/todo/cli-agent-send-argv.md` lands).

use std::sync::Arc;

use aura_cli::cli::{AgentCmd, Commands};
use aura_cli::{CliError, ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;

fn make_ctx(invocation: Invocation) -> aura_cli::CommandContext {
    let tmpdir = tempfile::tempdir().expect("tmpdir");
    let mut config = AuraConfig::default();
    config.workspace.path = tmpdir.path().to_string_lossy().into_owned();
    ContextBuilder::new(Arc::new(config))
        .workspace(Arc::new(aura_workspace::WorkspaceManager::new(
            tmpdir.path().to_path_buf(),
        )))
        .build()
        .with_invocation(invocation)
        .with_format(OutputFormat::Plain)
        .with_confirmed(true)
}

fn agent_send_cmd() -> Commands {
    Commands::Agent {
        cmd: AgentCmd::Send {
            session: "sess-1".into(),
            message: "hi".into(),
            yes: true,
        },
    }
}

#[tokio::test]
async fn agent_send_slash_mode_is_forbidden() {
    let ctx = make_ctx(Invocation::Slash);
    let err = dispatch::run(&ctx, agent_send_cmd())
        .await
        .expect_err("slash invocation must reject agent send");
    assert!(
        matches!(err, CliError::AgentSendForbiddenInSlash(ref name) if name == "agent send"),
        "expected AgentSendForbiddenInSlash, got {err:?}",
    );
}

#[tokio::test]
async fn agent_send_argv_mode_reports_deferred() {
    let ctx = make_ctx(Invocation::Argv);
    let err = dispatch::run(&ctx, agent_send_cmd())
        .await
        .expect_err("argv invocation must surface the deferred-runtime error today");
    let CliError::Manager(msg) = err else {
        panic!("expected CliError::Manager, got {err:?}");
    };
    assert!(
        msg.contains("agent send is not yet available in argv mode"),
        "expected deferred-runtime error string, got {msg:?}",
    );
}
