//! Parser-level tests for the clap command tree.
//!
//! These verify that argv is accepted (or rejected) without booting any
//! manager. Behavioural tests for individual commands live next to the
//! dispatcher in `dispatch_smoke.rs`.

use aura_cli::cli::{
    AgentCmd, ChannelCmd, Cli, Commands, ConfigCmd, CronCmd, JobCmd, JobStatusArg, LlmCmd,
    MemoryCmd, SessionCmd, ShellKind, SkillsCmd, TraceCmd, WorkspaceCmd,
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
fn channels_list_parses() {
    let cli = parse(&["channel", "list"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Channel {
            cmd: ChannelCmd::List
        })
    ));
}

#[test]
fn setup_takes_no_args() {
    let cli = parse(&["setup"]);
    assert!(matches!(cli.command, Some(Commands::Setup)));
}

#[test]
fn channels_bot_add_takes_no_args() {
    let cli = parse(&["channel", "add"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Channel {
            cmd: ChannelCmd::Add
        })
    ));

    // The positional `telegram` form was removed in favour of an
    // interactive picker; clap should reject any extra positional.
    assert!(Cli::try_parse_from(["aura", "channel", "add", "telegram"]).is_err());
}

#[test]
fn channel_bots_subcommand_removed() {
    assert!(Cli::try_parse_from(["aura", "channel", "bots"]).is_err());
}

#[test]
fn channel_remove_takes_no_args() {
    let cli = parse(&["channel", "remove"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Channel {
            cmd: ChannelCmd::Remove
        })
    ));

    assert!(Cli::try_parse_from(["aura", "channel", "remove", "telegram"]).is_err());
    assert!(Cli::try_parse_from(["aura", "channel", "remove", "telegram", "bot-1"]).is_err());
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
fn llm_subcommands_parse() {
    let cli = parse(&["llm", "probe"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm {
            cmd: LlmCmd::Probe { name: None }
        })
    ));
    let cli = parse(&["llm", "probe", "openai"]);
    match cli.command {
        Some(Commands::Llm {
            cmd: LlmCmd::Probe { name: Some(n) },
        }) => assert_eq!(n, "openai"),
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["llm", "live-model"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm {
            cmd: LlmCmd::LiveModel { name: None }
        })
    ));
    let cli = parse(&["llm", "add"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm { cmd: LlmCmd::Add })
    ));
    let cli = parse(&["llm", "edit"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm { cmd: LlmCmd::Edit })
    ));
    let cli = parse(&["llm", "remove"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm {
            cmd: LlmCmd::Remove
        })
    ));
    let cli = parse(&["llm", "default"]);
    assert!(matches!(
        cli.command,
        Some(Commands::Llm {
            cmd: LlmCmd::Default
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
fn workspace_set_identity_accepts_file_or_content_exclusively() {
    assert!(Cli::try_parse_from(["aura", "workspace", "set-identity"]).is_err());

    let cli = parse(&["workspace", "set-identity", "soul", "--content", "hello"]);
    match cli.command {
        Some(Commands::Workspace {
            cmd:
                WorkspaceCmd::SetIdentity {
                    name,
                    file,
                    content,
                    yes,
                },
        }) => {
            assert_eq!(name, "soul");
            assert!(file.is_none());
            assert_eq!(content.as_deref(), Some("hello"));
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&[
        "workspace",
        "set-identity",
        "identity",
        "--file",
        "/tmp/a.md",
        "-y",
    ]);
    match cli.command {
        Some(Commands::Workspace {
            cmd: WorkspaceCmd::SetIdentity {
                name, file, yes, ..
            },
        }) => {
            assert_eq!(name, "identity");
            assert_eq!(file.as_deref(), Some("/tmp/a.md"));
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // --file and --content are mutually exclusive.
    assert!(
        Cli::try_parse_from([
            "aura",
            "workspace",
            "set-identity",
            "soul",
            "--file",
            "x.md",
            "--content",
            "hi",
        ])
        .is_err()
    );
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
    // `mcp serve` (server side) stays deferred — only the client commands ship.
    assert!(Cli::try_parse_from(["aura", "mcp", "serve"]).is_err());
    // `mcp login` was folded into `mcp add` (auth runs inline).
    assert!(Cli::try_parse_from(["aura", "mcp", "login", "github"]).is_err());
    assert!(Cli::try_parse_from(["aura", "gateway"]).is_err());
    assert!(Cli::try_parse_from(["aura", "daemon", "start"]).is_err());
}

#[test]
fn mcp_add_http_parses() {
    use aura_cli::cli::{McpCmd, McpTransportArg};
    let cli = parse(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "github",
        "https://api.githubcopilot.com/mcp/",
    ]);
    match cli.command {
        Some(Commands::Mcp {
            cmd:
                McpCmd::Add {
                    transport,
                    name,
                    command_or_url,
                    ..
                },
        }) => {
            assert!(matches!(transport, McpTransportArg::Http));
            assert_eq!(name, "github");
            assert_eq!(command_or_url, "https://api.githubcopilot.com/mcp/");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn mcp_add_stdio_with_args_parses() {
    use aura_cli::cli::{McpCmd, McpTransportArg};
    let cli = parse(&[
        "mcp",
        "add",
        "demo",
        "npx",
        "--",
        "@modelcontextprotocol/server-memory",
    ]);
    match cli.command {
        Some(Commands::Mcp {
            cmd:
                McpCmd::Add {
                    transport,
                    name,
                    command_or_url,
                    args,
                    ..
                },
        }) => {
            assert!(matches!(transport, McpTransportArg::Stdio));
            assert_eq!(name, "demo");
            assert_eq!(command_or_url, "npx");
            assert_eq!(
                args,
                vec!["@modelcontextprotocol/server-memory".to_string()]
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn mcp_list_get_remove_parse() {
    use aura_cli::cli::McpCmd;
    // Default: probe is on (no_probe = false).
    let cli = parse(&["mcp", "list"]);
    match cli.command {
        Some(Commands::Mcp {
            cmd: McpCmd::List { no_probe },
        }) => assert!(!no_probe),
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["mcp", "list", "--no-probe"]);
    match cli.command {
        Some(Commands::Mcp {
            cmd: McpCmd::List { no_probe },
        }) => assert!(no_probe),
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["mcp", "get", "github"]);
    match cli.command {
        Some(Commands::Mcp {
            cmd: McpCmd::Get { name, no_probe },
        }) => {
            assert_eq!(name, "github");
            assert!(!no_probe);
        }
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["mcp", "get", "github", "--no-probe"]);
    match cli.command {
        Some(Commands::Mcp {
            cmd: McpCmd::Get { name, no_probe },
        }) => {
            assert_eq!(name, "github");
            assert!(no_probe);
        }
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["mcp", "remove", "github", "--yes"]);
    match cli.command {
        Some(Commands::Mcp {
            cmd: McpCmd::Remove { name, yes },
        }) => {
            assert_eq!(name, "github");
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
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
fn cron_mutating_subcommands_are_rejected() {
    for args in [
        &[
            "aura",
            "cron",
            "add",
            "-u",
            "alice",
            "-s",
            "0 9 * * *",
            "-p",
            "hi",
        ][..],
        &["aura", "cron", "show", "c1"],
        &["aura", "cron", "rm", "c1"],
        &["aura", "cron", "enable", "c1"],
        &["aura", "cron", "disable", "c1"],
        &["aura", "cron", "run", "c1"],
        &["aura", "cron", "runs", "--id", "c1"],
    ] {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "expected rejection of cron mutation subcommand: {args:?}"
        );
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
fn skills_search_accepts_optional_query() {
    let cli = parse(&["skills", "search"]);
    match cli.command {
        Some(Commands::Skills {
            cmd: SkillsCmd::Search { query },
        }) => assert!(query.is_none()),
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["skills", "search", "translate"]);
    match cli.command {
        Some(Commands::Skills {
            cmd: SkillsCmd::Search { query },
        }) => assert_eq!(query.as_deref(), Some("translate")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn skills_check_accepts_optional_name() {
    let cli = parse(&["skills", "check"]);
    match cli.command {
        Some(Commands::Skills {
            cmd: SkillsCmd::Check { name },
        }) => assert!(name.is_none()),
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["skills", "check", "summarize"]);
    match cli.command {
        Some(Commands::Skills {
            cmd: SkillsCmd::Check { name },
        }) => assert_eq!(name.as_deref(), Some("summarize")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn agent_send_requires_session_and_message() {
    assert!(Cli::try_parse_from(["aura", "agent", "send"]).is_err());
    assert!(
        Cli::try_parse_from(["aura", "agent", "send", "--session", "s1"]).is_err(),
        "--message is required"
    );
    assert!(
        Cli::try_parse_from(["aura", "agent", "send", "--message", "hi"]).is_err(),
        "--session is required"
    );

    let cli = parse(&["agent", "send", "--session", "sess-1", "--message", "hello"]);
    match cli.command {
        Some(Commands::Agent {
            cmd:
                AgentCmd::Send {
                    session,
                    message,
                    yes,
                },
        }) => {
            assert_eq!(session, "sess-1");
            assert_eq!(message, "hello");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["agent", "send", "--session", "s", "--message", "m", "-y"]);
    match cli.command {
        Some(Commands::Agent {
            cmd: AgentCmd::Send { yes, .. },
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
