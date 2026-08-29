//! Parser-level tests for the clap command tree.
//!
//! These verify that argv is accepted (or rejected) without booting any
//! manager. Behavioural tests for individual commands live next to the
//! dispatcher in `dispatch_smoke.rs`.

use baybo_cli::cli::{
    ChannelCmd, Cli, Commands, ConfigCmd, CostCmd, CronCmd, DeviceCmd, LlmCmd, LogCmd, SessionCmd,
    ShellKind, SkillsCmd, TurnCmd, TurnStatusArg,
};
use clap::Parser;

fn parse(argv: &[&str]) -> Cli {
    let mut full = vec!["baybo"];
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
    assert!(matches!(cli.command, Some(Commands::Status { .. })));

    let cli = parse(&["status", "--json"]);
    assert!(cli.global.json);
    assert!(matches!(cli.command, Some(Commands::Status { .. })));
}

#[test]
fn cost_show_scopes_are_mutually_exclusive() {
    let cli = parse(&["cost", "show"]);
    match cli.command {
        Some(Commands::Cost {
            cmd:
                CostCmd::Show {
                    user,
                    session,
                    turn,
                    since,
                    until,
                },
        }) => {
            assert!(user.is_none());
            assert!(session.is_none());
            assert!(turn.is_none());
            assert!(since.is_none());
            assert!(until.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["cost", "show", "--user", "u1"]);
    match cli.command {
        Some(Commands::Cost {
            cmd: CostCmd::Show { user, .. },
        }) => assert_eq!(user.as_deref(), Some("u1")),
        other => panic!("unexpected: {other:?}"),
    }

    // --user and --session / --turn are mutually exclusive
    assert!(
        Cli::try_parse_from(["baybo", "cost", "show", "--user", "u", "--session", "s"]).is_err()
    );
    assert!(
        Cli::try_parse_from(["baybo", "cost", "show", "--session", "s", "--turn", "j"]).is_err()
    );
}

#[test]
fn status_accepts_optional_live_flag() {
    let cli = parse(&["status"]);
    match cli.command {
        Some(Commands::Status { live }) => assert!(!live),
        other => panic!("unexpected: {other:?}"),
    }
    let cli = parse(&["status", "--live"]);
    match cli.command {
        Some(Commands::Status { live }) => assert!(live),
        other => panic!("unexpected: {other:?}"),
    }
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
    assert!(Cli::try_parse_from(["baybo", "skills", "info"]).is_err());
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
fn device_pair_parses_separate_proxy_and_push_urls() {
    let cli = parse(&[
        "device",
        "pair",
        "--proxy-url",
        "relay.example",
        "--push-url",
        "push.example",
        "--remote-api-key",
        "tenant-key",
    ]);
    match cli.command {
        Some(Commands::Device {
            cmd:
                DeviceCmd::Pair {
                    proxy_url,
                    push_url,
                    remote_api_key,
                },
        }) => {
            assert_eq!(proxy_url.as_deref(), Some("relay.example"));
            assert_eq!(push_url.as_deref(), Some("push.example"));
            assert_eq!(remote_api_key, "tenant-key");
        }
        other => panic!("unexpected: {other:?}"),
    }

    assert!(Cli::try_parse_from(["baybo", "device", "pair", "--relay-url", "old"]).is_err());
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
    assert!(Cli::try_parse_from(["baybo", "channel", "add", "telegram"]).is_err());
}

#[test]
fn channel_bots_subcommand_removed() {
    assert!(Cli::try_parse_from(["baybo", "channel", "bots"]).is_err());
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

    assert!(Cli::try_parse_from(["baybo", "channel", "remove", "telegram"]).is_err());
    assert!(Cli::try_parse_from(["baybo", "channel", "remove", "telegram", "bot-1"]).is_err());
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
fn workspace_is_no_longer_a_subcommand() {
    for argv in [
        &["baybo", "workspace"][..],
        &["baybo", "workspace", "show"][..],
        &[
            "baybo",
            "workspace",
            "set-identity",
            "soul",
            "--content",
            "hi",
        ][..],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "expected rejection of removed workspace subcommand: {argv:?}"
        );
    }
}

#[test]
fn completion_requires_shell_kind() {
    assert!(Cli::try_parse_from(["baybo", "completion"]).is_err());
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let cli = parse(&["completion", shell]);
        assert!(matches!(cli.command, Some(Commands::Completion { .. })));
    }
}

#[test]
fn completion_rejects_unknown_shell() {
    assert!(Cli::try_parse_from(["baybo", "completion", "nushell"]).is_err());
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
    let cli = parse(&["config", "validate", "--file", "baybo.json"]);
    match cli.command {
        Some(Commands::Config {
            cmd: ConfigCmd::Validate { file },
        }) => assert_eq!(file.as_deref(), Some("baybo.json")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unknown_subcommand_is_rejected() {
    // `mcp serve` (server side) stays deferred — only the client commands ship.
    assert!(Cli::try_parse_from(["baybo", "mcp", "serve"]).is_err());
    // `mcp login` was folded into `mcp add` (auth runs inline).
    assert!(Cli::try_parse_from(["baybo", "mcp", "login", "github"]).is_err());
    assert!(Cli::try_parse_from(["baybo", "gateway"]).is_err());
    assert!(Cli::try_parse_from(["baybo", "daemon", "start"]).is_err());
}

#[test]
fn mcp_add_http_parses() {
    use baybo_cli::cli::{McpCmd, McpTransportArg};
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
    use baybo_cli::cli::{McpCmd, McpTransportArg};
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
    use baybo_cli::cli::McpCmd;
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
    assert!(Cli::try_parse_from(["baybo", "session", "show"]).is_err());
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
    assert!(Cli::try_parse_from(["baybo", "session", "history"]).is_err());
    let cli = parse(&["session", "history", "sid-1"]);
    match cli.command {
        Some(Commands::Session {
            cmd:
                SessionCmd::History {
                    id,
                    include_superseded,
                    superseded_only,
                },
        }) => {
            assert_eq!(id, "sid-1");
            assert!(!include_superseded);
            assert!(!superseded_only);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn session_history_accepts_supersede_flags() {
    let cli = parse(&["session", "history", "sid", "--include-superseded"]);
    match cli.command {
        Some(Commands::Session {
            cmd:
                SessionCmd::History {
                    include_superseded,
                    superseded_only,
                    ..
                },
        }) => {
            assert!(include_superseded);
            assert!(!superseded_only);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["session", "history", "sid", "--superseded-only"]);
    match cli.command {
        Some(Commands::Session {
            cmd:
                SessionCmd::History {
                    include_superseded,
                    superseded_only,
                    ..
                },
        }) => {
            assert!(!include_superseded);
            assert!(superseded_only);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Mutually exclusive — clap should reject both at once.
    assert!(
        Cli::try_parse_from([
            "baybo",
            "session",
            "history",
            "sid",
            "--include-superseded",
            "--superseded-only"
        ])
        .is_err()
    );
}

#[test]
fn session_kill_is_no_longer_a_subcommand() {
    assert!(Cli::try_parse_from(["baybo", "session", "kill", "sid"]).is_err());
}

#[test]
fn turn_list_parses_without_filter() {
    let cli = parse(&["turn", "list"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::List { status },
        }) => assert!(status.is_none()),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn turn_list_accepts_status_filter() {
    let cli = parse(&["turn", "list", "--status", "in-progress"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::List { status },
        }) => assert!(matches!(status, Some(TurnStatusArg::InProgress))),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn turn_list_rejects_unknown_status() {
    assert!(Cli::try_parse_from(["baybo", "turn", "list", "--status", "bogus"]).is_err());
}

#[test]
fn turn_show_requires_id() {
    assert!(Cli::try_parse_from(["baybo", "turn", "show"]).is_err());
    let cli = parse(&["turn", "show", "jid-1"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::Show { id },
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
    assert!(Cli::try_parse_from(["baybo", "cron", "show"]).is_err());
    let cli = parse(&["cron", "show", "c1"]);
    match cli.command {
        Some(Commands::Cron {
            cmd: CronCmd::Show { id },
        }) => assert_eq!(id, "c1"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn cron_mutating_subcommands_are_rejected() {
    // `list` and `show` ship; create / delete / enable / disable / run
    // are LLM-only via the cron agent tools.
    for args in [
        &[
            "baybo",
            "cron",
            "add",
            "-u",
            "alice",
            "-s",
            "0 9 * * *",
            "-p",
            "hi",
        ][..],
        &["baybo", "cron", "rm", "c1"],
        &["baybo", "cron", "enable", "c1"],
        &["baybo", "cron", "disable", "c1"],
        &["baybo", "cron", "run", "c1"],
        &["baybo", "cron", "runs", "--id", "c1"],
    ] {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "expected rejection of cron mutation subcommand: {args:?}"
        );
    }
}

#[test]
fn memory_is_no_longer_a_subcommand() {
    for argv in [
        &["baybo", "memory"][..],
        &["baybo", "memory", "list"][..],
        &["baybo", "memory", "search", "rust"][..],
        &["baybo", "memory", "show", "mid"][..],
        &["baybo", "memory", "promote", "mid"][..],
        &["baybo", "memory", "clear", "--session", "sid"][..],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "expected rejection of removed memory subcommand: {argv:?}"
        );
    }
}

#[test]
fn turn_cancel_accepts_yes_flag() {
    let cli = parse(&["turn", "cancel", "jid"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::Cancel { id, yes },
        }) => {
            assert_eq!(id, "jid");
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["turn", "cancel", "jid", "--yes"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::Cancel { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["turn", "cancel", "jid", "-y"]);
    match cli.command {
        Some(Commands::Turn {
            cmd: TurnCmd::Cancel { yes, .. },
        }) => assert!(yes),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn session_export_accepts_out_and_yes() {
    let cli = parse(&["session", "export", "sess-2"]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Export { id, out, yes },
        }) => {
            assert_eq!(id, "sess-2");
            assert!(out.is_none());
            assert!(!yes);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&[
        "session",
        "export",
        "sess-3",
        "--out",
        "/tmp/t.json",
        "--yes",
    ]);
    match cli.command {
        Some(Commands::Session {
            cmd: SessionCmd::Export { id, out, yes },
        }) => {
            assert_eq!(id, "sess-3");
            assert_eq!(out.as_deref(), Some("/tmp/t.json"));
            assert!(yes);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn trace_top_level_is_gone() {
    // `trace`'s subcommands collapsed into `session`: show counts now
    // arrive via `session show`; exporting the call tree is `session
    // export`. Anything beginning with `trace` is unknown.
    for argv in [
        &["baybo", "trace"][..],
        &["baybo", "trace", "list"][..],
        &["baybo", "trace", "show", "sid"][..],
        &["baybo", "trace", "export", "sid"][..],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "trace folded into `session`: {argv:?}"
        );
    }
}

#[test]
fn config_get_requires_path() {
    assert!(Cli::try_parse_from(["baybo", "config", "get"]).is_err());
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
    assert!(Cli::try_parse_from(["baybo", "config", "set"]).is_err());
    assert!(Cli::try_parse_from(["baybo", "config", "set", "llm.model"]).is_err());
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
fn config_unset_requires_path() {
    assert!(Cli::try_parse_from(["baybo", "config", "unset"]).is_err());
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

#[test]
fn log_main_accepts_optional_date_and_limit() {
    let cli = parse(&["log", "main"]);
    match cli.command {
        Some(Commands::Log {
            cmd:
                LogCmd::Main {
                    date,
                    limit,
                    follow,
                },
        }) => {
            assert!(date.is_none());
            assert_eq!(limit, 200);
            assert!(!follow);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&["log", "main", "--date", "2026-05-11", "-n", "10", "-f"]);
    match cli.command {
        Some(Commands::Log {
            cmd:
                LogCmd::Main {
                    date,
                    limit,
                    follow,
                },
        }) => {
            assert_eq!(date.as_deref(), Some("2026-05-11"));
            assert_eq!(limit, 10);
            assert!(follow);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn log_channel_requires_channel_name() {
    assert!(Cli::try_parse_from(["baybo", "log", "channel"]).is_err());

    let cli = parse(&["log", "channel", "telegram"]);
    match cli.command {
        Some(Commands::Log {
            cmd:
                LogCmd::Channel {
                    channel,
                    date,
                    limit,
                    follow,
                },
        }) => {
            assert_eq!(channel, "telegram");
            assert!(date.is_none());
            assert_eq!(limit, 200);
            assert!(!follow);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = parse(&[
        "log",
        "channel",
        "slack",
        "--date",
        "2026-05-10",
        "--limit",
        "5",
        "--follow",
    ]);
    match cli.command {
        Some(Commands::Log {
            cmd:
                LogCmd::Channel {
                    channel,
                    date,
                    limit,
                    follow,
                },
        }) => {
            assert_eq!(channel, "slack");
            assert_eq!(date.as_deref(), Some("2026-05-10"));
            assert_eq!(limit, 5);
            assert!(follow);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn log_channel_rejects_path_traversal_and_other_unsafe_inputs() {
    // The `channel` positional becomes a filename component:
    //   <workspace>/logs/channel/<channel>.log.<date>
    // so anything other than `[a-z0-9_-]+` would either escape the
    // directory (`/`, `..`) or push the lookup somewhere unexpected
    // (uppercase, dots, unicode). The clap value-parser must reject
    // these at argv-parse time so neither the argv handler nor the
    // slash dispatcher ever builds the path.
    for argv in [
        &["baybo", "log", "channel", "../../../etc/passwd"][..],
        &["baybo", "log", "channel", "../etc"][..],
        &["baybo", "log", "channel", "tg/secret"][..],
        &["baybo", "log", "channel", "tg\\secret"][..],
        &["baybo", "log", "channel", "."][..],
        &["baybo", "log", "channel", ".."][..],
        &["baybo", "log", "channel", ".hidden"][..],
        &["baybo", "log", "channel", "tele.gram"][..],
        &["baybo", "log", "channel", "Telegram"][..], // uppercase
        &["baybo", "log", "channel", "tele gram"][..], // space
        &["baybo", "log", "channel", ""][..],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "expected rejection of unsafe channel name: {argv:?}"
        );
    }

    // Legitimate names still pass.
    for argv in [
        &["baybo", "log", "channel", "telegram"][..],
        &["baybo", "log", "channel", "slack"][..],
        &["baybo", "log", "channel", "tg_bot42"][..],
        &["baybo", "log", "channel", "openclaw-lark"][..],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_ok(),
            "expected accept of legitimate channel name: {argv:?}"
        );
    }
}
