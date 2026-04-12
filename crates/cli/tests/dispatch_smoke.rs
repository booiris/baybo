//! Smoke-tests for the dispatcher.
//!
//! Each test builds a `CommandContext` from in-memory defaults and runs a
//! read-only command end-to-end. These guard against regressions in the
//! wiring between clap, `dispatch::run`, and individual command handlers,
//! without spinning up the full bootstrap.

use std::path::PathBuf;
use std::sync::Arc;

use aura_channels::ChannelRegistry;
use aura_cli::cli::{ChannelsCmd, Commands, ConfigCmd, LlmCmd, SkillsCmd, ToolsCmd, WorkspaceCmd};
use aura_cli::{ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;
use aura_skills::SkillRegistry;
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use tokio::sync::RwLock;

fn context() -> aura_cli::CommandContext {
    let config = Arc::new(AuraConfig::default());
    ContextBuilder::new(config)
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
}

#[tokio::test]
async fn config_show_emits_json_shaped_payload() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Show { section: None },
        },
    )
    .await
    .expect("config show");
    let data = out.data.expect("structured payload");
    assert!(data.get("llm").is_some(), "llm section should be present");
    assert!(data.get("agent").is_some());
}

#[tokio::test]
async fn config_show_unknown_section_errors() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Show {
                section: Some("nope".into()),
            },
        },
    )
    .await
    .expect_err("unknown section should error");
    assert!(format!("{err}").contains("nope"));
}

#[tokio::test]
async fn config_schema_returns_default_shape() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Schema,
        },
    )
    .await
    .expect("config schema");
    assert!(out.data.is_some());
    assert!(!out.human.is_empty());
}

#[tokio::test]
async fn config_file_reports_no_config_when_unset() {
    let ctx = context();
    // SAFETY: test environment; removing the var is scoped to this process.
    unsafe {
        std::env::remove_var("AURA_CONFIG_PATH");
    }
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::File,
        },
    )
    .await
    .expect("config file");
    let data = out.data.expect("structured payload");
    assert_eq!(data["resolved"], false);
}

#[tokio::test]
async fn skills_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::List,
        },
    )
    .await
    .expect("skills list");
    assert!(out.human.contains("no skills"));
}

#[tokio::test]
async fn tools_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Tools {
            cmd: ToolsCmd::List,
        },
    )
    .await
    .expect("tools list");
    assert!(out.human.contains("no tools"));
}

#[tokio::test]
async fn channels_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Channels {
            cmd: ChannelsCmd::List,
        },
    )
    .await
    .expect("channels list");
    assert!(out.human.contains("no channels"));
}

#[tokio::test]
async fn llm_status_errors_when_client_missing() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Llm {
            cmd: LlmCmd::Status,
        },
    )
    .await
    .expect_err("llm status without client should fail");
    assert!(format!("{err}").to_lowercase().contains("llm"));
}

#[tokio::test]
async fn workspace_show_reports_identity_flags() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::Show,
        },
    )
    .await
    .expect("workspace show");
    let data = out.data.expect("structured payload");
    let files = data.get("identity_files").expect("identity_files key");
    for key in ["agents", "soul", "user", "identity"] {
        assert!(files.get(key).is_some(), "missing identity flag {key}");
    }
}

#[tokio::test]
async fn status_reports_zero_counts_on_defaults() {
    let ctx = context();
    let out = dispatch::run(&ctx, Commands::Status).await.expect("status");
    let data = out.data.expect("structured payload");
    assert_eq!(data["skills"], 0);
    assert_eq!(data["tools"], 0);
    assert_eq!(data["channels"], 0);
}

#[tokio::test]
async fn doctor_runs_and_aggregates_checks() {
    let ctx = context();
    let out = dispatch::run(&ctx, Commands::Doctor).await.expect("doctor");
    let data = out.data.expect("structured payload");
    assert!(data.get("status").is_some());
    let checks = data.get("checks").and_then(|v| v.as_array()).unwrap();
    assert!(
        checks.iter().any(|c| c["name"] == "llm.client"),
        "expected llm.client check row"
    );
}
