//! Parser-level tests for the clap command tree.
//!
//! These verify that argv is accepted (or rejected) without booting any
//! manager. Behavioural tests for individual commands live next to the
//! dispatcher in `dispatch_smoke.rs`.

use aura_cli::cli::{
    ChannelsCmd, Cli, Commands, ConfigCmd, LlmCmd, ShellKind, SkillsCmd, ToolsCmd, WorkspaceCmd,
};
use clap::Parser;

fn parse(argv: &[&str]) -> Cli {
    let mut full = vec!["aura"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).expect("argv should parse")
}

#[test]
fn bare_invocation_has_no_subcommand() {
    let cli = parse(&[]);
    assert!(cli.command.is_none());
    assert!(!cli.global.json);
    assert!(!cli.global.plain);
}

#[test]
fn config_show_accepts_optional_section() {
    let cli = parse(&["config", "show"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Config {
            cmd: ConfigCmd::Show { section: None }
        })
    ));
    let cli = parse(&["config", "show", "llm"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Show { section },
        }) => assert_eq!(section.as_deref(), Some("llm")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn json_flag_is_global() {
    let cli = parse(&["--json", "status"]);
    assert!(cli.global.json);
    assert!(matches!(cli.command, Some(Commands::Status)));

    let cli = parse(&["status", "--json"]);
    assert!(cli.global.json);
    assert!(matches!(cli.command, Some(Commands::Status)));
}

#[test]
fn plain_and_no_color_are_global_flags() {
    let cli = parse(&["--plain", "--no-color", "doctor"]);
    assert!(cli.global.plain);
    assert!(cli.global.no_color);
    assert!(matches!(cli.command, Some(Commands::Doctor)));
}

#[test]
fn skills_info_requires_name() {
    assert!(Cli::try_parse_from(["aura", "skills", "info"]).is_err());
    let cli = parse(&["skills", "info", "echo"]);
    match cli.command {
        Some(Commands::Skills {
            cmd: SkillsCmd::Info { name },
        }) => assert_eq!(name, "echo"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn tools_info_requires_name() {
    assert!(Cli::try_parse_from(["aura", "tools", "info"]).is_err());
    let cli = parse(&["tools", "info", "web.search"]);
    match cli.command {
        Some(Commands::Tools {
            cmd: ToolsCmd::Info { name },
        }) => assert_eq!(name, "web.search"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn channels_list_parses() {
    let cli = parse(&["channels", "list"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Channels {
            cmd: ChannelsCmd::List
        })
    ));
}

#[test]
fn llm_status_parses() {
    let cli = parse(&["llm", "status"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm {
            cmd: LlmCmd::Status
        })
    ));
}

#[test]
fn workspace_show_parses() {
    let cli = parse(&["workspace", "show"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Workspace {
            cmd: WorkspaceCmd::Show
        })
    ));
}

#[test]
fn completion_requires_shell_kind() {
    assert!(Cli::try_parse_from(["aura", "completion"]).is_err());
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let cli = parse(&["completion", shell]);
        assert!(matches!(cli.command, Some(Commands::Completion { .. })));
    }
}

#[test]
fn completion_rejects_unknown_shell() {
    assert!(Cli::try_parse_from(["aura", "completion", "nushell"]).is_err());
}

#[test]
fn shell_kind_round_trips_through_clap() {
    let cli = parse(&["completion", "zsh"]);
    match cli.command {
        Some(Commands::Completion { shell }) => assert!(matches!(shell, ShellKind::Zsh)),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn config_validate_accepts_optional_file() {
    let cli = parse(&["config", "validate"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Config {
            cmd: ConfigCmd::Validate { file: None }
        })
    ));
    let cli = parse(&["config", "validate", "--file", "aura.json"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Validate { file },
        }) => assert_eq!(file.as_deref(), Some("aura.json")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unknown_subcommand_is_rejected() {
    assert!(Cli::try_parse_from(["aura", "mcp", "serve"]).is_err());
    assert!(Cli::try_parse_from(["aura", "gateway"]).is_err());
    assert!(Cli::try_parse_from(["aura", "daemon", "start"]).is_err());
}
