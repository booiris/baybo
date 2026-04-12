//! Parser-level tests for the clap command tree.
//!
//! These verify that argv is accepted (or rejected) without booting any
//! manager. Behavioural tests for individual commands live next to the
//! dispatcher in `dispatch_smoke.rs`.

use aura_cli::cli::{
    ChannelsCmd, Cli, Commands, ConfigCmd, CronCmd, JobCmd, JobStatusArg, LlmCmd, MemoryCmd,
    SessionCmd, ShellKind, SkillsCmd, ToolsCmd, TraceCmd, WorkspaceCmd,
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

#[test]
fn session_list_parses() {
    let cli = parse(&["session", "list"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Session {
            cmd: SessionCmd::List
        })
    ));
}

#[test]
fn session_show_requires_id() {
    assert!(Cli::try_parse_from(["aura", "session", "show"]).is_err());
    let cli = parse(&["session", "show", "abc"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Show { id },
        }) => assert_eq!(id, "abc"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn session_history_requires_id() {
    assert!(Cli::try_parse_from(["aura", "session", "history"]).is_err());
    let cli = parse(&["session", "history", "sid-1"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::History { id },
        }) => assert_eq!(id, "sid-1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn session_kill_accepts_yes_flag() {
    let cli = parse(&["session", "kill", "sid"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Kill { id, yes },
        }) => {
            assert_eq!(id, "sid");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["session", "kill", "sid", "--yes"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Kill { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["session", "kill", "sid", "-y"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Kill { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn job_list_parses_without_filter() {
    let cli = parse(&["job", "list"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::List { status },
        }) => assert!(status.is_none()),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn job_list_accepts_status_filter() {
    let cli = parse(&["job", "list", "--status", "in-progress"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::List { status },
        }) => assert!(matches!(status, Some(JobStatusArg::InProgress))),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn job_list_rejects_unknown_status() {
    assert!(Cli::try_parse_from(["aura", "job", "list", "--status", "bogus"]).is_err());
}

#[test]
fn job_show_requires_id() {
    assert!(Cli::try_parse_from(["aura", "job", "show"]).is_err());
    let cli = parse(&["job", "show", "jid-1"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::Show { id },
        }) => assert_eq!(id, "jid-1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn cron_list_parses() {
    let cli = parse(&["cron", "list"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Cron { cmd: CronCmd::List })
    ));
}

#[test]
fn cron_show_requires_id() {
    assert!(Cli::try_parse_from(["aura", "cron", "show"]).is_err());
    let cli = parse(&["cron", "show", "cron-1"]);
    match cli.command {
        Some(Commands::Cron {
            cmd: CronCmd::Show { id },
        }) => assert_eq!(id, "cron-1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn cron_rm_accepts_yes_flag() {
    let cli = parse(&["cron", "rm", "c1"]);
    match cli.command {
        Some(Commands::Cron {
            cmd: CronCmd::Rm { id, yes },
        }) => {
            assert_eq!(id, "c1");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["cron", "rm", "c1", "--yes"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Cron {
            cmd: CronCmd::Rm { yes: true, .. }
        })
    ));
}

#[test]
fn cron_enable_disable_parse() {
    let cli = parse(&["cron", "enable", "c1"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Cron {
            cmd: CronCmd::Enable { .. }
        })
    ));
    let cli = parse(&["cron", "disable", "c1"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Cron {
            cmd: CronCmd::Disable { .. }
        })
    ));
}

#[test]
fn cron_run_requires_yes_flag_only_in_slash_mode() {
    let cli = parse(&["cron", "run", "c1"]);
    match cli.command {
        Some(Commands::Cron {
            cmd: CronCmd::Run { id, yes },
        }) => {
            assert_eq!(id, "c1");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["cron", "run", "c1", "-y"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Cron {
            cmd: CronCmd::Run { yes: true, .. }
        })
    ));
}

#[test]
fn cron_runs_requires_id_flag() {
    assert!(Cli::try_parse_from(["aura", "cron", "runs"]).is_err());
    assert!(Cli::try_parse_from(["aura", "cron", "runs", "c1"]).is_err());
    let cli = parse(&["cron", "runs", "--id", "c1"]);
    match cli.command {
        Some(Commands::Cron {
            cmd: CronCmd::Runs { id },
        }) => assert_eq!(id, "c1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn memory_list_accepts_optional_user_and_limit() {
    let cli = parse(&["memory", "list"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::List { user, limit },
        }) => {
            assert!(user.is_none());
            assert_eq!(limit, 50);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["memory", "list", "--user", "u1", "--limit", "5"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::List { user, limit },
        }) => {
            assert_eq!(user.as_deref(), Some("u1"));
            assert_eq!(limit, 5);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn memory_search_requires_query() {
    assert!(Cli::try_parse_from(["aura", "memory", "search"]).is_err());
    let cli = parse(&["memory", "search", "rust"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::Search { query, user, limit },
        }) => {
            assert_eq!(query, "rust");
            assert!(user.is_none());
            assert_eq!(limit, 20);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn memory_show_requires_id() {
    assert!(Cli::try_parse_from(["aura", "memory", "show"]).is_err());
    let cli = parse(&["memory", "show", "mid"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::Show { id },
        }) => assert_eq!(id, "mid"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn memory_promote_defaults_to_pin() {
    let cli = parse(&["memory", "promote", "mid"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::Promote { id, to, yes },
        }) => {
            assert_eq!(id, "mid");
            assert!((to - 1.0).abs() < f32::EPSILON);
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["memory", "promote", "mid", "--to", "0.75", "-y"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::Promote { to, yes, .. },
        }) => {
            assert!((to - 0.75).abs() < f32::EPSILON);
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn memory_clear_requires_session_flag() {
    assert!(Cli::try_parse_from(["aura", "memory", "clear"]).is_err());
    assert!(Cli::try_parse_from(["aura", "memory", "clear", "sid"]).is_err());
    let cli = parse(&["memory", "clear", "--session", "sid"]);
    match cli.command {
        Some(Commands::Memory {
            cmd: MemoryCmd::Clear { session, yes },
        }) => {
            assert_eq!(session, "sid");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn job_cancel_accepts_yes_flag() {
    let cli = parse(&["job", "cancel", "jid"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::Cancel { id, yes },
        }) => {
            assert_eq!(id, "jid");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["job", "cancel", "jid", "--yes"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::Cancel { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["job", "cancel", "jid", "-y"]);
    match cli.command {
        Some(Commands::Job {
            cmd: JobCmd::Cancel { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn trace_list_defaults_and_session_flag() {
    let cli = parse(&["trace", "list"]);
    match cli.command {
        Some(Commands::Trace {
            cmd: TraceCmd::List { session, limit },
        }) => {
            assert!(session.is_none());
            assert_eq!(limit, 50);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["trace", "list", "--session", "sid-7", "--limit", "10"]);
    match cli.command {
        Some(Commands::Trace {
            cmd: TraceCmd::List { session, limit },
        }) => {
            assert_eq!(session.as_deref(), Some("sid-7"));
            assert_eq!(limit, 10);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn trace_show_requires_id() {
    assert!(Cli::try_parse_from(["aura", "trace", "show"]).is_err());
    let cli = parse(&["trace", "show", "sess-1"]);
    match cli.command {
        Some(Commands::Trace {
            cmd: TraceCmd::Show { id },
        }) => assert_eq!(id, "sess-1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn trace_export_accepts_out_and_yes() {
    let cli = parse(&["trace", "export", "sess-1"]);
    match cli.command {
        Some(Commands::Trace {
            cmd: TraceCmd::Export { id, out, yes },
        }) => {
            assert_eq!(id, "sess-1");
            assert!(out.is_none());
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["trace", "export", "sess-2", "--out", "/tmp/t.json", "--yes"]);
    match cli.command {
        Some(Commands::Trace {
            cmd: TraceCmd::Export { id, out, yes },
        }) => {
            assert_eq!(id, "sess-2");
            assert_eq!(out.as_deref(), Some("/tmp/t.json"));
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn config_get_requires_path() {
    assert!(Cli::try_parse_from(["aura", "config", "get"]).is_err());
    let cli = parse(&["config", "get", "llm.model"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Get { path },
        }) => assert_eq!(path, "llm.model"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn config_set_requires_path_and_value() {
    assert!(Cli::try_parse_from(["aura", "config", "set"]).is_err());
    assert!(Cli::try_parse_from(["aura", "config", "set", "llm.model"]).is_err());
    let cli = parse(&["config", "set", "llm.model", "gpt-5"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Set { path, value, yes },
        }) => {
            assert_eq!(path, "llm.model");
            assert_eq!(value, "gpt-5");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["config", "set", "llm.model", "gpt-5", "-y"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Set { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn config_unset_requires_path() {
    assert!(Cli::try_parse_from(["aura", "config", "unset"]).is_err());
    let cli = parse(&["config", "unset", "llm.model", "--yes"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Unset { path, yes },
        }) => {
            assert_eq!(path, "llm.model");
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
}
