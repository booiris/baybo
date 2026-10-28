//! `baybo session search` against a real sqlite store: the FTS index lives
//! only there, so the fakes cannot stand in for it.

use std::sync::Arc;

use baybo_cli::cli::{SessionCmd, SessionSearchArgs};
use baybo_cli::{
    CliError, CommandContext, CommandOutput, Commands, ContextBuilder, Invocation, OutputFormat,
};
use baybo_config::BayboConfig;
use baybo_model::{ChannelType, ChatMessage, ContentBlock, SessionId, TriggerSource, User};
use baybo_session::SessionManager;
use baybo_session::search::{
    DEFAULT_SEARCH_LIMIT, MAX_HITS_PER_SESSION, SEARCH_SCAN_LIMIT, SessionSearch,
};
use serde_json::Value;

const TERM: &str = "迁移";

struct Fixture {
    _tmpdir: tempfile::TempDir,
    ctx: CommandContext,
    owner: SessionId,
    telegram: SessionId,
    hidden: SessionId,
    archived: SessionId,
    cron_workspace: SessionId,
}

impl Fixture {
    fn visible(&self) -> [&str; 2] {
        [self.owner.as_str(), self.telegram.as_str()]
    }
}

fn user(channel: &ChannelType) -> User {
    User {
        id: "u".into(),
        name: None,
        channel: channel.clone(),
    }
}

fn one_shot_cron() -> TriggerSource {
    TriggerSource::Cron {
        cron_job_id: "cj-1".into(),
        origin_session_id: None,
        conversation: false,
        job_title: None,
        project_id: None,
    }
}

async fn seeded() -> Fixture {
    let tmpdir = tempfile::tempdir().unwrap();
    let stores = baybo_storage::Store::open(tmpdir.path().join("baybo.db"))
        .await
        .unwrap();
    let manager = Arc::new(SessionManager::new(
        stores.session.clone(),
        stores.session_folder.clone(),
    ));

    let mut ids = Vec::new();
    for (channel, trigger, texts) in [
        (
            ChannelType::owner(),
            TriggerSource::User,
            vec![
                "数据库的迁移怎么做",
                "先看 schema 再说",
                "迁移 第二次",
                "迁移 第三次",
            ],
        ),
        (
            ChannelType::telegram(),
            TriggerSource::User,
            vec!["数据库 迁移 要停机吗"],
        ),
        (
            ChannelType::owner(),
            TriggerSource::User,
            vec!["这个迁移被藏起来了"],
        ),
        (
            ChannelType::owner(),
            TriggerSource::User,
            vec!["这个迁移被归档了"],
        ),
        (
            ChannelType::owner(),
            one_shot_cron(),
            vec!["定时任务里的迁移"],
        ),
    ] {
        let session = manager
            .create_session_with_trigger(user(&channel), channel.clone(), trigger)
            .await
            .unwrap();
        for text in texts {
            manager
                .append_session_message(
                    &session.id,
                    &ChatMessage::user(vec![ContentBlock::Text(text.into())]),
                )
                .await
                .unwrap();
        }
        ids.push(session.id);
    }
    let [owner, telegram, hidden, archived, cron_workspace]: [SessionId; 5] =
        ids.try_into().unwrap();
    manager.set_hidden(&hidden, true).await.unwrap();
    manager.set_archived(&archived, true).await.unwrap();

    let ctx = ContextBuilder::new(Arc::new(BayboConfig::default()))
        .session(Arc::clone(&manager))
        .session_search(Arc::new(SessionSearch::new(
            stores.message_search.clone(),
            stores.session.clone(),
        )))
        .build()
        .with_invocation(Invocation::Argv);
    Fixture {
        _tmpdir: tmpdir,
        ctx,
        owner,
        telegram,
        hidden,
        archived,
        cron_workspace,
    }
}

fn args(query: &[&str]) -> SessionSearchArgs {
    SessionSearchArgs {
        query: query.iter().map(|s| (*s).to_owned()).collect(),
        channel: None,
        session: None,
        include_hidden: false,
        include_archived: false,
        include_cron_workspaces: false,
        all: false,
        limit: DEFAULT_SEARCH_LIMIT,
        per_session: MAX_HITS_PER_SESSION,
        scan: SEARCH_SCAN_LIMIT,
    }
}

async fn run_search(ctx: &CommandContext, args: SessionSearchArgs) -> CommandOutput {
    baybo_cli::run(
        ctx,
        Commands::Session {
            cmd: SessionCmd::Search(args),
        },
    )
    .await
    .expect("search")
}

async fn search(ctx: &CommandContext, args: SessionSearchArgs) -> Value {
    run_search(ctx, args)
        .await
        .data
        .expect("search always carries data")
}

fn session_ids(data: &Value) -> Vec<&str> {
    data["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| g["session_id"].as_str().expect("session_id"))
        .collect()
}

fn sorted(mut ids: Vec<&str>) -> Vec<&str> {
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn default_scope_reaches_every_channel_but_only_listed_conversations() {
    let fx = seeded().await;
    let data = search(&fx.ctx, args(&[TERM])).await;

    assert_eq!(
        sorted(session_ids(&data)),
        sorted(fx.visible().to_vec()),
        "{data}"
    );
    let channels: Vec<&str> = data["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["channel"].as_str().unwrap())
        .collect();
    assert!(channels.contains(&"owner") && channels.contains(&"telegram"));
    assert_eq!(data["truncated"], false);
    assert_eq!(data["sessions_matched"], 2);
    assert_eq!(data["query"], TERM);
}

#[tokio::test]
async fn each_include_flag_opens_exactly_its_own_axis() {
    let fx = seeded().await;
    for (flag, extra) in [
        (
            SessionSearchArgs {
                include_hidden: true,
                ..args(&[TERM])
            },
            fx.hidden.as_str(),
        ),
        (
            SessionSearchArgs {
                include_archived: true,
                ..args(&[TERM])
            },
            fx.archived.as_str(),
        ),
        (
            SessionSearchArgs {
                include_cron_workspaces: true,
                ..args(&[TERM])
            },
            fx.cron_workspace.as_str(),
        ),
    ] {
        let data = search(&fx.ctx, flag).await;
        let mut expected = fx.visible().to_vec();
        expected.push(extra);
        assert_eq!(sorted(session_ids(&data)), sorted(expected), "{data}");
    }

    let data = search(
        &fx.ctx,
        SessionSearchArgs {
            all: true,
            ..args(&[TERM])
        },
    )
    .await;
    assert_eq!(session_ids(&data).len(), 5, "{data}");
    assert_eq!(data["scope"]["include_hidden"], true);
    assert_eq!(data["scope"]["include_archived"], true);
    assert_eq!(data["scope"]["include_cron_workspaces"], true);
}

#[tokio::test]
async fn channel_and_session_narrow_the_scope() {
    let fx = seeded().await;
    let data = search(
        &fx.ctx,
        SessionSearchArgs {
            channel: Some("owner".into()),
            ..args(&[TERM])
        },
    )
    .await;
    assert_eq!(session_ids(&data), [fx.owner.as_str()], "{data}");
    assert_eq!(data["scope"]["channel"], "owner");

    let data = search(
        &fx.ctx,
        SessionSearchArgs {
            session: Some(fx.telegram.as_str().to_owned()),
            ..args(&[TERM])
        },
    )
    .await;
    assert_eq!(session_ids(&data), [fx.telegram.as_str()], "{data}");

    // Naming the hidden session does not lift the hidden rule.
    let data = search(
        &fx.ctx,
        SessionSearchArgs {
            session: Some(fx.hidden.as_str().to_owned()),
            ..args(&[TERM])
        },
    )
    .await;
    assert!(session_ids(&data).is_empty(), "{data}");
}

#[tokio::test]
async fn limit_knobs_reach_the_search_and_the_output_says_what_was_cut() {
    let fx = seeded().await;
    let out = run_search(
        &fx.ctx,
        SessionSearchArgs {
            limit: 1,
            per_session: 1,
            scan: 2,
            ..args(&[TERM])
        },
    )
    .await;
    let data = out.data.clone().unwrap();
    assert_eq!(
        data["limits"],
        serde_json::json!({ "sessions": 1, "per_session": 1, "scan": 2 })
    );
    assert_eq!(data["truncated"], true, "{data}");
    assert_eq!(data["groups"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(data["groups"][0]["hits"].as_array().unwrap().len(), 1);
    assert!(
        out.human.contains("scan window full (2 rows)"),
        "{}",
        out.human
    );

    // A wide scan with a tight session cap: the cap, not the window, is
    // what cut the list, and the header says so.
    let out = run_search(
        &fx.ctx,
        SessionSearchArgs {
            limit: 1,
            per_session: 1,
            ..args(&[TERM])
        },
    )
    .await;
    let data = out.data.clone().unwrap();
    assert_eq!(data["truncated"], false, "{data}");
    assert_eq!(data["sessions_matched"], 2);
    assert_eq!(session_ids(&data), [fx.owner.as_str()], "{data}");
    assert_eq!(data["groups"][0]["total_hits"], 3);
    assert_eq!(data["groups"][0]["hits"].as_array().unwrap().len(), 1);
    assert!(
        out.human.contains("showing 1 of 2: raise -n"),
        "{}",
        out.human
    );
    assert!(out.human.contains("… and 2 more"), "{}", out.human);
}

#[tokio::test]
async fn words_are_anded_and_hits_carry_the_full_prose() {
    let fx = seeded().await;
    let data = search(&fx.ctx, args(&["数据库", "停机"])).await;
    assert_eq!(session_ids(&data), [fx.telegram.as_str()], "{data}");
    let hit = &data["groups"][0]["hits"][0];
    assert_eq!(hit["text"], "数据库 迁移 要停机吗");
    assert_eq!(hit["role"], "user");
    assert_eq!(hit["ordinal"], 0);
    assert!(hit["superseded_by"].is_null());
    assert!(hit["score"].is_number());
    assert_eq!(data["groups"][0]["total_hits"], 1);
    assert!(data["groups"][0]["session_title"].is_null());

    // Latin keeps word semantics, prefix-widened to reach suffixes.
    let data = search(&fx.ctx, args(&["schem"])).await;
    assert_eq!(session_ids(&data), [fx.owner.as_str()], "{data}");
}

#[tokio::test]
async fn rendered_output_follows_the_requested_format() {
    let fx = seeded().await;
    let out = run_search(
        &fx.ctx,
        SessionSearchArgs {
            session: Some(fx.telegram.as_str().to_owned()),
            ..args(&[TERM])
        },
    )
    .await;

    let plain = out.render(OutputFormat::Plain);
    assert!(
        plain.starts_with(&format!("1 session matches \"{TERM}\" (1 hit)\n")),
        "{plain}"
    );
    assert!(
        plain.contains(&format!("scope: channel=any  session={}", fx.telegram)),
        "{plain}"
    );
    assert!(plain.contains("  #0     user "), "{plain}");
    assert!(plain.contains("数据库 迁移 要停机吗"), "{plain}");
    assert!(plain.contains("(rows carry \"ordinal\")"), "{plain}");

    let json: Value = serde_json::from_str(&out.render(OutputFormat::Json)).expect("json");
    assert_eq!(json["groups"][0]["session_id"], fx.telegram.as_str());
}

#[tokio::test]
async fn the_footer_names_a_real_round_trip_to_history() {
    let fx = seeded().await;
    let data = search(
        &fx.ctx,
        SessionSearchArgs {
            session: Some(fx.owner.as_str().to_owned()),
            ..args(&["schema"])
        },
    )
    .await;
    let ordinal = data["groups"][0]["hits"][0]["ordinal"].as_i64().unwrap();

    let history = baybo_cli::run(
        &fx.ctx,
        Commands::Session {
            cmd: SessionCmd::History {
                id: fx.owner.as_str().to_owned(),
                include_superseded: true,
                superseded_only: false,
            },
        },
    )
    .await
    .expect("history");
    let rows = history.data.unwrap();
    let row = rows["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["ordinal"].as_i64() == Some(ordinal))
        .expect("the ordinal addresses a history row");
    assert_eq!(row["message"]["content"][0]["Text"], "先看 schema 再说");
    assert!(history.human.contains(&format!("[#{ordinal}]")));
}

#[tokio::test]
async fn no_match_prints_the_closed_axes() {
    let fx = seeded().await;
    let out = run_search(&fx.ctx, args(&["nothing-here"])).await;
    assert!(
        out.human.starts_with("no matches for \"nothing-here\"\n"),
        "{}",
        out.human
    );
    assert!(
        out.human.contains("hint: widen with --include-hidden"),
        "{}",
        out.human
    );
    assert!(out.data.unwrap()["groups"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn argv_light_boot_reports_the_missing_service() {
    let ctx = ContextBuilder::new(Arc::new(BayboConfig::default()))
        .build()
        .with_invocation(Invocation::Argv);
    let err = baybo_cli::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::Search(args(&["x"])),
        },
    )
    .await
    .expect_err("no search service wired");
    assert!(matches!(err, CliError::Manager(_)), "{err}");
}
