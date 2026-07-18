//! Integration coverage for the admin-side `/v1/chat/*` REST surface.
//!
//! Spins a tower-style admin router (no TCP listener) and walks the
//! happy path: create session → list shows it → get returns transcript →
//! DELETE hides the row (the session itself stays on the server, only the
//! chat list filters it) → unhide restores it to the default listing.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_channels::ChannelKind;
use baybo_config::ChannelsConfig;
use baybo_gateway::auth::{AuthedClient, DEVICE_ID_HEADER};
use baybo_gateway::channel::boot;
use baybo_gateway::server::build_admin_router_for_tests;
use baybo_gateway::test_support::build_test_deps;
use baybo_model::{ChannelType, ChatMessage, ContentBlock, SessionId, User};
use baybo_store::{DeviceRow, DeviceStatus};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn chat_api_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    // ── 1. Create a session ────────────────────────────────────────
    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    assert!(
        cred.get("channel_token").is_none(),
        "web chat now reuses the admin bearer instead of minting a channel token",
    );
    assert!(
        cred.get("channel_token_header").is_none(),
        "no channel-token header is returned",
    );

    // ── 2. List shows it ───────────────────────────────────────────
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|row| row["session_id"].as_str() == Some(session_id.as_str())),
        "list should contain the just-created session: {items:?}",
    );

    // ── 3. Get returns transcript (empty, but the row exists) ───────
    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["session_id"].as_str(), Some(session_id.as_str()));
    assert!(
        detail["transcript"]
            .as_array()
            .is_some_and(|a| a.is_empty()),
        "transcript is empty on a fresh session",
    );

    // ── 4. Slash manifest exposes /compact but hides /new ───────────
    let manifest = get(&router, "/v1/chat/slash-manifest", StatusCode::OK).await;
    let manifest_items = manifest["items"].as_array().expect("items");
    assert!(
        !manifest_items.is_empty(),
        "/v1/chat/slash-manifest exposes the gateway's slash commands",
    );
    let commands: Vec<&str> = manifest_items
        .iter()
        .map(|c| c["command"].as_str().expect("command"))
        .collect();
    assert!(
        commands.contains(&"compact"),
        "manifest must advertise /compact, got {commands:?}",
    );
    assert!(
        !commands.contains(&"new"),
        "web composer should not see /new — it has a 'New chat' button instead, got {commands:?}",
    );

    // ── 5. DELETE hides — row stays live ───────────────────────────
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/chat/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // GET still returns the row, with hidden = true.
    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["hidden"].as_bool(), Some(true));

    // Default list omits it…
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .all(|row| row["session_id"].as_str() != Some(session_id.as_str())),
        "hidden session should be filtered from default list: {items:?}",
    );

    // …and `?include_hidden=true` brings it back.
    let list_inc = get(
        &router,
        "/v1/chat/sessions?include_hidden=true",
        StatusCode::OK,
    )
    .await;
    let items_inc = list_inc["items"].as_array().expect("items");
    let hidden_row = items_inc
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("hidden row should show up under include_hidden");
    assert_eq!(hidden_row["hidden"].as_bool(), Some(true));

    // ── 6. Unhide brings it back into the default list ──────────────
    let unhide = post(
        &router,
        &format!("/v1/chat/sessions/{session_id}/unhide"),
        Body::empty(),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(unhide.is_null());
    let list_after = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items_after = list_after["items"].as_array().expect("items");
    assert!(
        items_after
            .iter()
            .any(|row| row["session_id"].as_str() == Some(session_id.as_str())),
        "session must show up again after unhide",
    );
}

#[tokio::test]
async fn chat_archive_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();

    // The list ALWAYS serializes `archived` — even when false. Clients
    // without an archived view rely on the field being present on every
    // row; `archived: false` must not be dropped the way `hidden: false`
    // is.
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("created session listed")
        .clone();
    assert_eq!(
        row["archived"].as_bool(),
        Some(false),
        "archived must serialize as an explicit false, not be omitted: {row:?}",
    );

    // Archive → 204, and the row STAYS in the default list (no
    // server-side filtering — clients group archived rows themselves).
    let archived = put(
        &router,
        &format!("/v1/chat/sessions/{session_id}/archive"),
        Body::from(json!({ "archived": true }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(archived.is_null());

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("archived session must still be listed")
        .clone();
    assert_eq!(row["archived"].as_bool(), Some(true));

    // Unarchive restores the flag.
    put(
        &router,
        &format!("/v1/chat/sessions/{session_id}/archive"),
        Body::from(json!({ "archived": false }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("session listed after unarchive")
        .clone();
    assert_eq!(row["archived"].as_bool(), Some(false));

    // Unknown session → 404.
    put(
        &router,
        "/v1/chat/sessions/no-such-session/archive",
        Body::from(json!({ "archived": true }).to_string()),
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn chat_create_accepts_client_supplied_session_id() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    let requested = "client-session-1";

    let created = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": requested }).to_string()),
        StatusCode::OK,
    )
    .await;
    assert_eq!(created["session_id"].as_str(), Some(requested));

    let duplicate = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": requested }).to_string()),
        StatusCode::OK,
    )
    .await;
    assert_eq!(duplicate["session_id"].as_str(), Some(requested));

    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{requested}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["session_id"].as_str(), Some(requested));
}

/// Seed one session with a completed tool-using turn:
/// user(0, "run it") → tool_use(1) → tool_result(2) → reply(3, "done").
async fn seed_tool_turn_session(
    tg: &baybo_gateway::test_support::TestGateway,
    router: &axum::Router,
) -> String {
    let cred = post(router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    let sid = SessionId::from(session_id.as_str());
    let rows = [
        ChatMessage::user(vec![ContentBlock::Text("run it".into())])
            .with_platform_msg_id("client-msg-1"),
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
            signature: None,
        }]),
        ChatMessage::tool_result("c1".to_owned(), "hi".to_owned()),
        ChatMessage::assistant(vec![ContentBlock::Text("done".into())]),
    ];
    for msg in rows {
        tg.deps
            .session_manager
            .append_session_message(&sid, &msg)
            .await
            .expect("append");
    }
    session_id
}

/// The chat-list summary carries a Telegram-style second-line preview
/// (`last_message_text`) that follows the WHOLE conversation: it settles on the
/// newest displayable message regardless of author, skipping the turn's
/// text-less tool rows, while `last_user_text` stays the user-only label the
/// web sidebar uses.
#[tokio::test]
async fn chat_list_last_message_text_follows_latest_reply() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    // user("run it") → tool_use → tool_result → reply("done")
    let session_id = seed_tool_turn_session(&tg, &router).await;

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("seeded session listed")
        .clone();

    // Newest displayable message is the assistant's final answer — the
    // intervening tool_use / tool_result rows carry no text and are skipped.
    assert_eq!(
        row["last_message_text"].as_str(),
        Some("done"),
        "preview must follow the conversation to the latest reply: {row:?}",
    );
    // The user-only label is unchanged: still the last human turn.
    assert_eq!(row["last_user_text"].as_str(), Some("run it"));
}

/// The second-line preview mirrors the transcript's bubble rules: a turn that
/// ends on tool activity (narration + tool_use + tool_result, no tool-free
/// final answer) shows NO assistant bubble, so the preview must fall back to
/// the user prompt rather than surface the mid-turn narration text.
#[tokio::test]
async fn chat_list_last_message_text_skips_mid_turn_narration() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    let sid = SessionId::from(session_id.as_str());
    // Newest tail rows are an assistant narration+tool row then a tool result —
    // neither renders a bubble, so the preview must be the user prompt.
    let rows = [
        ChatMessage::user(vec![ContentBlock::Text("run it".into())]),
        ChatMessage::assistant(vec![
            ContentBlock::Text("let me check the logs".into()),
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Bash".into(),
                input: json!({"command": "cat log"}),
                signature: None,
            },
        ]),
        ChatMessage::tool_result("c1".to_owned(), "…".to_owned()),
    ];
    for msg in rows {
        tg.deps
            .session_manager
            .append_session_message(&sid, &msg)
            .await
            .expect("append");
    }

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("seeded session listed")
        .clone();
    assert_eq!(
        row["last_message_text"].as_str(),
        Some("run it"),
        "narration/tool rows must not surface as the preview: {row:?}",
    );
}

/// A `/stop`-salvaged assistant reply carries the model-facing cancelled-turn
/// marker folded into its text; the preview must strip it (the same contract
/// the transcript renderer honours) so the list never shows internal framing.
#[tokio::test]
async fn chat_list_last_message_text_strips_cancelled_marker() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    let sid = SessionId::from(session_id.as_str());
    let salvaged = format!(
        "here is the partial{}",
        baybo_context::prompts::cancelled_turn::SUFFIX
    );
    let rows = [
        ChatMessage::user(vec![ContentBlock::Text("do the thing".into())]),
        ChatMessage::assistant(vec![ContentBlock::Text(salvaged)]),
    ];
    for msg in rows {
        tg.deps
            .session_manager
            .append_session_message(&sid, &msg)
            .await
            .expect("append");
    }

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("seeded session listed")
        .clone();
    assert_eq!(
        row["last_message_text"].as_str(),
        Some("here is the partial"),
        "cancelled-turn marker must be stripped from the preview: {row:?}",
    );
}

#[tokio::test]
async fn chat_sync_difference_is_full_fidelity_with_coverage_watermark() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    let session_id = seed_tool_turn_session(&tg, &router).await;

    // Difference from a cursor below the whole turn: full fidelity —
    // the user bubble, the reconstructed work block, and the reply.
    let sync = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync?since_ordinal=-1"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(sync["rebased"].as_bool(), Some(false));
    let rows = sync["rows"].as_array().expect("rows");
    let kinds: Vec<&str> = rows
        .iter()
        .map(|item| item["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["message", "work", "message"],
        "sync difference carries work blocks like every other read path: {rows:?}",
    );
    assert_eq!(rows[0]["role"].as_str(), Some("user"));
    assert_eq!(rows[0]["platform_msg_id"].as_str(), Some("client-msg-1"));
    assert_eq!(rows[0]["id"].as_str(), Some("m0"));
    assert_eq!(rows[1]["id"].as_str(), Some("w1"));
    let steps = rows[1]["steps"].as_array().expect("work steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["tool"].as_str(), Some("Bash"));
    assert_eq!(steps[0]["tool_status"].as_str(), Some("ok"));
    assert_eq!(rows[2]["id"].as_str(), Some("m3"));
    assert_eq!(rows[2]["text"].as_str(), Some("done"));
    assert_eq!(
        sync["next_cursor"].as_i64(),
        Some(3),
        "coverage watermark is the newest scanned ordinal"
    );
    // Difference responses carry no backfill floor — the client keeps its own.
    assert!(sync["oldest_ordinal"].is_null());
    assert_eq!(sync["has_more_older"].as_bool(), Some(false));

    // A caught-up cursor returns an empty page but still reports the
    // watermark, never null (null means "empty session").
    let idle = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync?since_ordinal=3"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(idle["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(idle["next_cursor"].as_i64(), Some(3));
    assert_eq!(idle["rebased"].as_bool(), Some(false));
}

#[tokio::test]
async fn chat_sync_watermark_covers_invisible_tail() {
    // Rows persisted after the last visible reply (internal/tool rows)
    // must still advance the coverage watermark — otherwise every sync
    // re-scans the invisible tail forever.
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    let sid = SessionId::from(session_id.as_str());
    let rows = [
        ChatMessage::user(vec![ContentBlock::Text("run it".into())]),
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
            signature: None,
        }]),
        ChatMessage::tool_result("c1".to_owned(), "hi".to_owned()),
    ];
    for msg in rows {
        tg.deps
            .session_manager
            .append_session_message(&sid, &msg)
            .await
            .expect("append");
    }

    let sync = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync?since_ordinal=-1"),
        StatusCode::OK,
    )
    .await;
    let rows = sync["rows"].as_array().expect("rows");
    // The unfinished turn reconstructs partially: the user bubble plus
    // the open work block (no reply yet).
    assert_eq!(rows[0]["kind"].as_str(), Some("message"));
    assert_eq!(
        sync["next_cursor"].as_i64(),
        Some(2),
        "watermark covers the invisible tool tail, not just visible rows",
    );
}

#[tokio::test]
async fn chat_sync_baseline_and_rebase_replace_with_newest_page() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    let session_id = seed_tool_turn_session(&tg, &router).await;

    // No cursor → newest-page baseline, not marked rebased.
    let baseline = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(baseline["rebased"].as_bool(), Some(false));
    assert_eq!(baseline["next_cursor"].as_i64(), Some(3));
    assert_eq!(baseline["has_more_older"].as_bool(), Some(false));
    assert_eq!(
        baseline["rows"].as_array().map(Vec::len),
        Some(3),
        "baseline is the full-fidelity newest page"
    );

    // A difference wider than `limit` (counted in emitted rows: this
    // turn emits 3) rebases onto the newest page instead.
    let rebased = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync?since_ordinal=-1&limit=2"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(rebased["rebased"].as_bool(), Some(true));
    assert_eq!(rebased["next_cursor"].as_i64(), Some(3));
    let page = rebased["rows"].as_array().expect("rows");
    assert!(!page.is_empty(), "rebase answers with the newest page");
    assert_eq!(
        rebased["oldest_ordinal"].as_i64(),
        Some(2),
        "REPLACE pages carry the backfill floor (limit=2 spans raw rows 2..3)"
    );
    assert_eq!(
        rebased["has_more_older"].as_bool(),
        Some(true),
        "older history stays fetchable below the rebased baseline"
    );
}

#[tokio::test]
async fn chat_sync_redelivers_control_events_anchored_at_cursor() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    let session_id = seed_tool_turn_session(&tg, &router).await;
    let sid = SessionId::from(session_id.as_str());

    // A notice written later, anchored at the newest ordinal (3) — an
    // ordinal a caught-up client already holds.
    tg.deps
        .session_manager
        .append_control_event(
            &sid,
            3,
            baybo_model::ControlEventKind::NoticeInfo,
            "compacted",
            chrono::Utc::now(),
            "",
        )
        .await
        .expect("append control event");

    // Sync from cursor 3 selects control events at `>=` the cursor, so
    // the anchored-at-cursor notice is (re)delivered; the client dedups
    // by its stable `n<seq>` id.
    let sync = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/sync?since_ordinal=3"),
        StatusCode::OK,
    )
    .await;
    let rows = sync["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "the anchored notice re-delivers: {rows:?}");
    assert_eq!(rows[0]["kind"].as_str(), Some("notice"));
    assert_eq!(rows[0]["id"].as_str(), Some("n0"));
    assert!(
        rows[0]["ordinal"].is_null(),
        "control events are not ordinal-addressed"
    );
    assert_eq!(rows[0]["text"].as_str(), Some("compacted"));
}

#[tokio::test]
async fn chat_message_point_lookup_probes_durability() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    let session_id = seed_tool_turn_session(&tg, &router).await;

    let found = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/messages?platform_msg_id=client-msg-1"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(found["found"].as_bool(), Some(true));
    assert_eq!(found["ordinal"].as_i64(), Some(0));

    let absent = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/messages?platform_msg_id=never-sent"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(absent["found"].as_bool(), Some(false));
    assert!(absent["ordinal"].is_null());

    get(
        &router,
        &format!("/v1/chat/sessions/{session_id}/messages?platform_msg_id="),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn chat_list_unread_count_reflects_read_cursor() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());
    // user(0) → tool_use(1) → tool_result(2) → final assistant reply(3, "done").
    let session_id = seed_tool_turn_session(&tg, &router).await;

    let unread_of = |list: &Value| -> i64 {
        list["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["session_id"] == json!(session_id))
            .expect("session in list")["unread_count"]
            .as_i64()
            .expect("unread_count")
    };

    // Nothing read yet: one unread final reply (the intermediate tool-using
    // assistant row at ordinal 1 does NOT count — only the tool-free reply).
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    assert_eq!(unread_of(&list), 1, "one unread assistant reply");

    // Mark read up to the newest ordinal → caught up.
    put(
        &router,
        &format!("/v1/chat/sessions/{session_id}/read"),
        Body::from(json!({ "ordinal": 3 }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    assert_eq!(unread_of(&list), 0, "read cursor cleared the badge");

    // A new reply after the read cursor bumps it back to unread.
    let sid = SessionId::from(session_id.as_str());
    tg.deps
        .session_manager
        .append_session_message(
            &sid,
            &ChatMessage::user(vec![ContentBlock::Text("again".into())]),
        )
        .await
        .expect("append user");
    tg.deps
        .session_manager
        .append_session_message(
            &sid,
            &ChatMessage::assistant(vec![ContentBlock::Text("sure".into())]),
        )
        .await
        .expect("append reply");
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    assert_eq!(unread_of(&list), 1, "a reply above the cursor is unread");

    // Max-wins: a stale lower ordinal must not regress the cursor / re-hide.
    put(
        &router,
        &format!("/v1/chat/sessions/{session_id}/read"),
        Body::from(json!({ "ordinal": 5 }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    put(
        &router,
        &format!("/v1/chat/sessions/{session_id}/read"),
        Body::from(json!({ "ordinal": 2 }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    assert_eq!(unread_of(&list), 0, "read cursor never regresses");
}

/// Web and device share the one `owner` chat channel, so a device-authed list
/// (as forwarded from the relay tunnel) returns every owner session — including
/// ones the web operator created. There is no per-surface universe to scope to.
#[tokio::test]
async fn chat_list_returns_every_owner_session_for_a_device_caller() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let device_session = tg
        .deps
        .session_manager
        .create_session(
            User {
                id: "device-1".into(),
                name: None,
                channel: ChannelType::owner(),
            },
            ChannelType::owner(),
        )
        .await
        .unwrap();
    tg.deps
        .session_manager
        .append_session_message(
            &device_session.id,
            &ChatMessage::user(vec![ContentBlock::Text("from device".into())]),
        )
        .await
        .unwrap();

    let http = tg
        .deps
        .session_manager
        .create_session(
            User {
                id: "web".into(),
                name: None,
                channel: ChannelType::owner(),
            },
            ChannelType::owner(),
        )
        .await
        .unwrap();
    tg.deps
        .session_manager
        .append_session_message(
            &http.id,
            &ChatMessage::user(vec![ContentBlock::Text("from web".into())]),
        )
        .await
        .unwrap();

    let router = build_router(build_admin_state(&tg));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/chat/sessions")
                .extension(AuthedClient::Device {
                    device_id: "device-1".into(),
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    let list: Value = serde_json::from_slice(&bytes).expect("response is json");
    let ids: Vec<&str> = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["session_id"].as_str().expect("session_id"))
        .collect();
    // Unified owner pool: both the device-origin and the http-origin session
    // appear in the device-scoped list — they are one universe now.
    assert!(
        ids.contains(&device_session.id.as_str()),
        "device session must be listed: {ids:?}",
    );
    assert!(
        ids.contains(&http.id.as_str()),
        "http session must also appear in the merged owner pool: {ids:?}",
    );
}

/// A recurring fire scheduled from the phone opens its conversation on the
/// *device* channel, so it must appear in the iOS chat list — that list is
/// where the fire's result is read, and the push deep-links into it. The
/// one-shot's private workspace stays out, on this channel as on any other.
///
/// The device-scoped counterpart of
/// `recurring_fire_conversations_are_listed_and_one_shot_sessions_are_not`: the
/// chat list is channel-scoped, so proving the rule on `http` proves nothing
/// about the client that actually reads it.
#[tokio::test]
async fn a_recurring_fire_scheduled_from_the_phone_is_listed_on_the_phone() {
    use baybo_model::TriggerSource;

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    // A cron job created from an iOS chat carries that session's channel, so
    // its fires are minted on `device` (see `Router::handle_cron_trigger`).
    let phone = User {
        id: "device-1".into(),
        name: None,
        channel: ChannelType::owner(),
    };
    let mut fires = Vec::new();
    for conversation in [true, false] {
        let fire = tg
            .deps
            .session_manager
            .create_session_with_trigger(
                phone.clone(),
                ChannelType::owner(),
                TriggerSource::Cron {
                    cron_job_id: "cj-news".into(),
                    origin_session_id: None,
                    conversation,
                    job_title: Some("Morning brief".into()),
                },
            )
            .await
            .expect("create cron fire session on the device channel");
        fires.push(fire.id.to_string());
    }
    let (recurring_id, one_shot_id) = (fires[0].clone(), fires[1].clone());

    // Exactly what the iOS client fetches: `GET /v1/chat/sessions` under a
    // device identity (see `app/ios/ffi/src/gateway_api.rs`).
    let router = build_router(build_admin_state(&tg));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/chat/sessions")
                .extension(AuthedClient::Device {
                    device_id: "device-1".into(),
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    let list: Value = serde_json::from_slice(&bytes).expect("response is json");
    let listed: Vec<&str> = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["session_id"].as_str().expect("session_id"))
        .collect();

    assert!(
        listed.contains(&recurring_id.as_str()),
        "a recurring fire's conversation must reach the phone's chat list, got {listed:?}",
    );
    assert!(
        !listed.contains(&one_shot_id.as_str()),
        "a one-shot's private workspace must not, got {listed:?}",
    );
}

/// A cron group's label is the job's **live** title while the job exists, and
/// the title snapshotted onto the fire once it doesn't — the two halves of the
/// rule in `docs/cron-groups.md`. Proving the live half is what proves a job
/// rename will propagate with no rewrite of any session; proving the tombstone
/// half is what proves deleting a noisy job doesn't spill its history back into
/// the flat list unnamed.
#[tokio::test]
async fn a_cron_group_is_labelled_by_the_live_job_title_and_falls_back_to_the_snapshot() {
    use baybo_cron::NewCronJob;
    use baybo_model::{CronSchedule, TriggerSource};

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let operator = User {
        id: baybo_gateway::auth::WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::owner(),
    };
    let job = tg
        .deps
        .cron_scheduler
        .create_job(NewCronJob {
            user_id: operator.id.clone(),
            channel: ChannelType::owner(),
            title: "Morning brief".into(),
            schedule: CronSchedule::Cron {
                expr: "0 8 * * *".into(),
            },
            prompt: "brief me".into(),
            timezone: "UTC".into(),
            origin_session_id: None,
        })
        .await
        .expect("create cron job");

    // The fire snapshots the title it was minted under — deliberately a
    // DIFFERENT string from the job's live one, so the assertions below can
    // tell which source the label actually came from.
    let fire = tg
        .deps
        .session_manager
        .create_session_with_trigger(
            operator.clone(),
            ChannelType::owner(),
            TriggerSource::Cron {
                cron_job_id: job.id.clone(),
                origin_session_id: None,
                conversation: true,
                job_title: Some("the name it was fired under".into()),
            },
        )
        .await
        .expect("create recurring fire");

    let router = build_router(build_admin_state(&tg));
    let row_for = |list: &Value, id: &str| -> Value {
        list["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|row| row["session_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("session {id} missing from the chat list"))
    };

    let row = row_for(
        &get(&router, "/v1/chat/sessions", StatusCode::OK).await,
        fire.id.as_str(),
    );
    assert_eq!(
        row["cron_job_id"], job.id,
        "a listed fire must carry the job it groups under",
    );
    assert_eq!(
        row["cron_job_title"], "Morning brief",
        "the LIVE job title wins over the fire's snapshot — this is what makes a \
         rename propagate with no session rewrite",
    );

    // Delete the job. The history must stay grouped, under the name it had.
    tg.deps
        .cron_scheduler
        .delete_job(&job.id)
        .await
        .expect("delete cron job");

    let row = row_for(
        &get(&router, "/v1/chat/sessions", StatusCode::OK).await,
        fire.id.as_str(),
    );
    assert_eq!(
        row["cron_job_id"], job.id,
        "a deleted job must not un-group its history",
    );
    assert_eq!(
        row["cron_job_title"], "the name it was fired under",
        "with no live job left, the label falls back to the fire's snapshot",
    );
}

/// A fire minted before the snapshot existed, whose job has since been deleted,
/// can be named from neither source. It must come back **ungrouped** rather than
/// with a `cron_job_id` no client can label — clients leave such a row flat.
#[tokio::test]
async fn a_pre_snapshot_fire_whose_job_is_gone_has_no_group_label() {
    use baybo_model::TriggerSource;

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let operator = User {
        id: baybo_gateway::auth::WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::owner(),
    };
    let orphan = tg
        .deps
        .session_manager
        .create_session_with_trigger(
            operator,
            ChannelType::owner(),
            TriggerSource::Cron {
                cron_job_id: "cj-long-gone".into(),
                origin_session_id: None,
                conversation: true,
                job_title: None,
            },
        )
        .await
        .expect("create legacy fire");

    let router = build_router(build_admin_state(&tg));
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let row = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["session_id"] == orphan.id.as_str())
        .expect("the fire is still listed");

    assert!(
        row.get("cron_job_title").is_none(),
        "an unnameable group must not be labelled, got {row:?}",
    );
}

/// The bulk mark-read behind a cron group's swipe action. A chat-list client
/// holds no ordinals, so "fully read" has to be resolved server-side from each
/// session's own tail — that is the whole reason this route exists instead of
/// the client looping over `PUT /read`.
#[tokio::test]
async fn marking_a_batch_read_clears_every_named_session() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let router = build_router(build_admin_state(&tg));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let created = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
        let id = created["session_id"]
            .as_str()
            .expect("session_id")
            .to_owned();
        let sid = SessionId::from(id.as_str());
        tg.deps
            .session_manager
            .append_session_message(
                &sid,
                &ChatMessage::assistant(vec![ContentBlock::Text("hi".into())]),
            )
            .await
            .expect("persist an unread reply");
        ids.push(id);
    }

    let unread_for = |list: &Value, id: &str| -> i64 {
        list["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|row| row["session_id"] == id)
            .and_then(|row| row["unread_count"].as_i64())
            .unwrap_or_else(|| panic!("session {id} missing from the chat list"))
    };
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    for id in &ids {
        assert_eq!(
            unread_for(&list, id),
            1,
            "each session starts with one unread"
        );
    }

    post(
        &router,
        "/v1/chat/sessions/read",
        Body::from(json!({ "session_ids": ids }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    for id in &ids {
        assert_eq!(
            unread_for(&list, id),
            0,
            "one batch call must clear every named session, not just the first",
        );
    }
}

/// The bulk hide behind a cron group's delete swipe. "Delete the group" means
/// "clear its execution records", which is one hide per fire — and, exactly like
/// the per-session `DELETE`, every row survives on the server.
#[tokio::test]
async fn hiding_a_batch_removes_every_named_session_from_the_list() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let router = build_router(build_admin_state(&tg));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let created = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
        ids.push(
            created["session_id"]
                .as_str()
                .expect("session_id")
                .to_owned(),
        );
    }

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    for id in &ids {
        assert!(
            list["items"]
                .as_array()
                .expect("items")
                .iter()
                .any(|row| row["session_id"] == id.as_str()),
            "session {id} must start out listed",
        );
    }

    post(
        &router,
        "/v1/chat/sessions/hide",
        Body::from(json!({ "session_ids": ids }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    for id in &ids {
        assert!(
            !list["items"]
                .as_array()
                .expect("items")
                .iter()
                .any(|row| row["session_id"] == id.as_str()),
            "one batch call must hide every named session, not just the first",
        );
    }

    // Hidden, never deleted: the rows are core data and stay recoverable.
    let list = get(
        &router,
        "/v1/chat/sessions?include_hidden=true",
        StatusCode::OK,
    )
    .await;
    for id in &ids {
        let row = list["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|row| row["session_id"] == id.as_str())
            .unwrap_or_else(|| panic!("session {id} must survive the hide"));
        assert_eq!(row["hidden"], json!(true), "got {row:?}");
    }
}

#[tokio::test]
async fn admin_device_header_creates_and_lists_device_sessions() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let router = build_admin_router_for_tests(&tg.deps);
    let device_key = device_proto::delegation::generate_signing_key();
    let device_id = device_proto::delegation::device_id_for(&device_key.verifying_key());

    let created = authed_device_request(
        &router,
        "POST",
        "/v1/chat/sessions",
        &tg.deps.admin_token,
        &device_id,
        StatusCode::OK,
    )
    .await;
    let session_id = created["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let session = tg
        .deps
        .session_manager
        .get(&SessionId::from(session_id.as_str()))
        .await
        .unwrap()
        .expect("created session");
    // Fully collapsed: a device-created session lives on the shared `owner`
    // pool (no per-surface provenance), under the shared `OWNER` identity.
    assert_eq!(session.channel, ChannelType::owner());
    assert_eq!(session.user.id, baybo_gateway::auth::OWNER_USER_ID);
    assert_eq!(session.user.channel, ChannelType::owner());

    let list = authed_device_request(
        &router,
        "GET",
        "/v1/chat/sessions",
        &tg.deps.admin_token,
        &device_id,
        StatusCode::OK,
    )
    .await;
    let items = list["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|row| row["session_id"].as_str() == Some(session_id.as_str())),
        "device-scoped list should contain the created device session: {items:?}",
    );

    let detail = authed_device_request(
        &router,
        "GET",
        &format!("/v1/chat/sessions/{session_id}"),
        &tg.deps.admin_token,
        &device_id,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["session_id"].as_str(), Some(session_id.as_str()));

    let web_response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/chat/sessions/{session_id}"))
                .header("authorization", format!("Bearer {}", tg.deps.admin_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    // Unified owner pool: a plain web identity CAN open a device-origin
    // session now — web and device are one universe.
    assert_eq!(
        web_response.status(),
        StatusCode::OK,
        "web identity must see device-origin sessions in the merged owner pool",
    );
    let web_detail: Value = serde_json::from_slice(
        &body::to_bytes(web_response.into_body(), 64 * 1024)
            .await
            .expect("body bytes"),
    )
    .expect("web detail json");
    assert_eq!(web_detail["session_id"].as_str(), Some(session_id.as_str()));
}

#[tokio::test]
async fn device_apns_token_api_persists_registration() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_admin_router_for_tests(&tg.deps);
    let device_key = device_proto::delegation::generate_signing_key();
    let device_id = device_proto::delegation::device_id_for(&device_key.verifying_key());

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/mobile/apns-token")
                .header("authorization", format!("Bearer {}", tg.deps.admin_token))
                .header(DEVICE_ID_HEADER, &device_id)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "apns_token": "new-token",
                        "apns_env": "production",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let secret = tg
        .deps
        .secret_vault
        .get_secret(&format!("device.{device_id}.apns"))
        .await
        .expect("vault read")
        .expect("apns registration persisted");
    let reg: Value = serde_json::from_slice(secret.as_bytes()).expect("registration json");
    assert_eq!(reg["apns_token"].as_str(), Some("new-token"));
    assert_eq!(reg["apns_env"].as_str(), Some("production"));
}

#[tokio::test]
async fn approved_device_token_with_header_creates_device_session() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let device_key = device_proto::delegation::generate_signing_key();
    let device_id = device_proto::delegation::device_id_for(&device_key.verifying_key());
    tg.deps
        .stores
        .device
        .create(&approved_device(&device_id, "approved-device-token"))
        .await
        .expect("seed approved device");

    let router = build_admin_router_for_tests(&tg.deps);
    let created = authed_device_request(
        &router,
        "POST",
        "/v1/chat/sessions",
        "approved-device-token",
        &device_id,
        StatusCode::OK,
    )
    .await;
    let session_id = created["session_id"].as_str().expect("session_id");
    let session = tg
        .deps
        .session_manager
        .get(&SessionId::from(session_id))
        .await
        .unwrap()
        .expect("created session");
    // Fully collapsed: the session lives on the shared `owner` pool (no
    // per-surface provenance), under the shared `OWNER` identity.
    assert_eq!(session.channel, ChannelType::owner());
    assert_eq!(session.user.id, baybo_gateway::auth::OWNER_USER_ID);
}

// ── helpers ─────────────────────────────────────────────────────────

fn build_admin_state(
    tg: &baybo_gateway::test_support::TestGateway,
) -> baybo_gateway::server::AdminState {
    baybo_gateway::server::AdminState {
        config: Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        message_search: tg.deps.stores.message_search.clone(),
        query_api: Arc::new(baybo_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.job_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: Arc::clone(&tg.deps.skill_registry),
        tool_registry: Arc::clone(&tg.deps.tool_registry),
        channel_registry: Arc::clone(&tg.deps.channel_registry),
        llm_pool: tg.deps.llm_pool.clone(),
        supervisor: tg.deps.supervisor.clone(),
        config_reloader: tg.deps.config_reloader.clone(),
        log_buffer: Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        agent_profile_store: tg.deps.stores.agent_profile.clone(),
        blob_store: tg.deps.stores.blob.clone(),
        channel_control: Arc::clone(&tg.deps.channel_control),
        secret_vault: Arc::clone(&tg.deps.secret_vault),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    }
}

fn build_router(state: baybo_gateway::server::AdminState) -> axum::Router {
    let (router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    router.with_state(state)
}

fn approved_device(device_id: &str, auth_token: &str) -> DeviceRow {
    DeviceRow {
        device_id: device_id.into(),
        device_pubkey: vec![0u8; 32],
        auth_token: auth_token.into(),
        status: DeviceStatus::Approved,
        rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
        created_at: 1,
        approved_at: Some(2),
        last_seen_at: None,
        relay_url: "wss://relay.test".into(),
        remote_api_key: "inst-test".into(),
    }
}

async fn post(router: &axum::Router, uri: &str, body: Body, expected: StatusCode) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "POST {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response is json")
}

async fn put(router: &axum::Router, uri: &str, body: Body, expected: StatusCode) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "PUT {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response is json")
}

async fn get(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "GET {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).expect("response is json")
}

async fn authed_device_request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    admin_token: &str,
    device_id: &str,
    expected: StatusCode,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {admin_token}"))
                .header(DEVICE_ID_HEADER, device_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "{method} {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response is json")
}

// Per-session model switch: `PUT /v1/chat/sessions/{id}/model` rejects
// an unknown entry name, persists a valid pin (no live actor in this
// harness, so it takes the persist-directly branch), surfaces it on the
// session detail, and clears it on `null`.
#[tokio::test]
async fn set_session_model_validates_persists_and_clears() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();

    // The stub pool keys its single entry by the model id, so the active
    // model id (from `GET /v1/llm`) doubles as the valid entry name.
    let llm = get(&router, "/v1/llm", StatusCode::OK).await;
    let valid_name = llm["model_id"].as_str().expect("model_id").to_owned();

    let model_uri = format!("/v1/chat/sessions/{session_id}/model");

    // Unknown name → 400, and the pin stays unset.
    put(
        &router,
        &model_uri,
        Body::from(r#"{"llm":"definitely-not-a-configured-entry"}"#),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // Valid name → 200; no live actor here, so it persists directly.
    let set = put(
        &router,
        &model_uri,
        Body::from(format!(r#"{{"llm":"{valid_name}"}}"#)),
        StatusCode::OK,
    )
    .await;
    assert_eq!(set["last_llm"].as_str(), Some(valid_name.as_str()));
    assert_eq!(
        set["applied_to_live_actor"].as_bool(),
        Some(false),
        "no actor is live in the gateway test harness",
    );

    // The pin shows up on the session detail.
    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["last_llm"].as_str(), Some(valid_name.as_str()));

    // `null` clears the pin back to default-llm.
    let cleared = put(
        &router,
        &model_uri,
        Body::from(r#"{"llm":null}"#),
        StatusCode::OK,
    )
    .await;
    assert!(
        cleared.get("last_llm").is_none_or(Value::is_null),
        "cleared pin must serialize as absent/null: {cleared:?}",
    );
    let detail_after = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}"),
        StatusCode::OK,
    )
    .await;
    assert!(
        detail_after.get("last_llm").is_none_or(Value::is_null),
        "session detail must drop last_llm after clear: {detail_after:?}",
    );
}

// Sidebar preview: `list_sessions` must surface each session's most
// recent user-authored text under `last_user_text`, with multi-line
// content collapsed to a single line. The web client uses this as the
// row label and drops it on `null` for sessions that have no user
// turn yet — both branches are exercised here.
#[tokio::test]
async fn list_sessions_exposes_last_user_text_preview() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    // Two sessions: the first will get a transcript, the second stays
    // empty so the response covers the "no user turn yet" branch.
    let with_text = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let with_text_id = with_text["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let empty = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let empty_id = empty["session_id"].as_str().expect("session_id").to_owned();

    // Append two user turns + an interleaved assistant turn — the
    // preview must surface the *latest* user message and reach past
    // any trailing assistant row that landed on top of it.
    let sid = SessionId::from(with_text_id.as_str());
    let rows: &[ChatMessage] = &[
        ChatMessage::user(vec![ContentBlock::Text("first ask".into())]),
        ChatMessage::assistant(vec![ContentBlock::Text("first reply".into())]),
        ChatMessage::user(vec![ContentBlock::Text(
            "second\nask\nwith   newlines".into(),
        )]),
        ChatMessage::assistant(vec![ContentBlock::Text("second reply".into())]),
    ];
    for msg in rows {
        tg.deps
            .session_manager
            .append_session_message(&sid, msg)
            .await
            .expect("append");
    }

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    let with_row = items
        .iter()
        .find(|row| row["session_id"].as_str() == Some(with_text_id.as_str()))
        .expect("with-text row");
    assert_eq!(
        with_row["last_user_text"].as_str(),
        Some("second ask with newlines"),
        "preview must surface the latest user turn with collapsed whitespace, got {with_row:?}",
    );
    let empty_row = items
        .iter()
        .find(|row| row["session_id"].as_str() == Some(empty_id.as_str()))
        .expect("empty row");
    assert!(
        empty_row.get("last_user_text").is_none() || empty_row["last_user_text"].is_null(),
        "empty session has no preview, got {empty_row:?}",
    );
}

// A recurring fire's session IS the conversation the user reads its result in,
// so it is listed and attachable like any other. A one-shot's session is a
// private workspace — its result is reported into the conversation that
// scheduled it — so it stays out of the list and cannot be attached to. The
// opt-in `?include_cron=true` query is the operator escape hatch that shows
// both.
#[tokio::test]
async fn recurring_fire_conversations_are_listed_and_one_shot_sessions_are_not() {
    use baybo_model::{ChannelType, TriggerSource, User};

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let user_cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let user_id = user_cred["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    // The fire runs as the same web operator that scheduled it — attaching to
    // its conversation is only allowed for that identity.
    let operator = User {
        id: baybo_gateway::auth::WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::owner(),
    };
    let mut ids = Vec::new();
    for conversation in [true, false] {
        let session = tg
            .deps
            .session_manager
            .create_session_with_trigger(
                operator.clone(),
                ChannelType::owner(),
                TriggerSource::Cron {
                    cron_job_id: "cj-test".into(),
                    origin_session_id: None,
                    conversation,
                    job_title: Some("Morning brief".into()),
                },
            )
            .await
            .expect("create cron session");
        ids.push(session.id.to_string());
    }
    let (recurring_id, one_shot_id) = (ids[0].clone(), ids[1].clone());

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    let listed = |id: &str| {
        items
            .iter()
            .any(|row| row["session_id"].as_str() == Some(id))
    };
    assert!(
        listed(&user_id),
        "user session must show up in default list"
    );
    assert!(
        listed(&recurring_id),
        "a recurring fire's conversation is a first-class chat, got {items:?}",
    );
    assert!(
        !listed(&one_shot_id),
        "a one-shot fire session has no conversation to show, got {items:?}",
    );

    let list_inc = get(
        &router,
        "/v1/chat/sessions?include_cron=true",
        StatusCode::OK,
    )
    .await;
    let items_inc = list_inc["items"].as_array().expect("items");
    assert!(
        items_inc
            .iter()
            .any(|row| row["session_id"].as_str() == Some(one_shot_id.as_str())),
        "include_cron=true is the operator view: it shows even the private fire sessions",
    );

    // Attaching: a recurring fire's conversation can be continued (the user
    // replies to what the fire reported); a one-shot's workspace cannot.
    post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": recurring_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": one_shot_id }).to_string()),
        StatusCode::NOT_FOUND,
    )
    .await;
}

// Channel kind sanity: the shared owner pool channel — the fan-out domain the
// `http`/`device` surfaces both resolve to — must be `Subscribed`.
#[test]
fn owner_pool_channel_kind_is_subscribed() {
    let reg = Arc::new(baybo_channels::ChannelRegistry::new());
    let cfg = ChannelsConfig::default();
    boot::install_channels(&reg, &cfg).expect("install");
    let ch = reg
        .get(&baybo_model::ChannelType::owner())
        .expect("owner pool channel exists");
    assert_eq!(ch.kind(), ChannelKind::Subscribed);
}

// Bonus: multi-attach + fan-out smoke test directly against the
// Channel API (no WS), confirming the design invariant that the same
// `session_id` can have N connections and an emission reaches all of
// them.
#[tokio::test]
async fn channel_multi_attach_fans_out_to_all_subscribers() {
    use baybo_channels::wire::Frame;
    use baybo_channels::{
        AgentEvent, AgentOutput, Channel, ChannelKind, Connection, ConnectionSink, OutgoingMessage,
        SendOutcome, SessionEvent,
    };
    use baybo_model::{ChannelType, MessageMetadata};
    use parking_lot::Mutex;

    struct VecSink {
        events: Arc<Mutex<Vec<SessionEvent>>>,
    }
    impl ConnectionSink for VecSink {
        fn try_send_event(&self, event: SessionEvent) -> SendOutcome {
            self.events.lock().push(event);
            SendOutcome::Sent
        }
        fn try_send_frame(&self, _frame: Frame) -> SendOutcome {
            SendOutcome::Sent
        }
    }

    let channel = Arc::new(Channel::new(
        ChannelType::owner(),
        ChannelKind::Subscribed,
        None,
    ));
    let bucket_a = Arc::new(Mutex::new(Vec::new()));
    let bucket_b = Arc::new(Mutex::new(Vec::new()));

    let conn_a = Arc::new(Connection::new(Arc::new(VecSink {
        events: Arc::clone(&bucket_a),
    })));
    let conn_b = Arc::new(Connection::new(Arc::new(VecSink {
        events: Arc::clone(&bucket_b),
    })));
    let id_a = conn_a.id();
    let id_b = conn_b.id();

    channel.attach(Arc::clone(&conn_a));
    channel.attach(Arc::clone(&conn_b));
    let view = channel.as_subscribed().expect("http channel is Subscribed");
    view.subscribe(id_a, "sess-shared".into()).unwrap();
    view.subscribe(id_b, "sess-shared".into()).unwrap();

    let outgoing = OutgoingMessage {
        session_id: "sess-shared".into(),
        user_id: "u1".into(),
        channel: ChannelType::owner(),
        content: vec![baybo_model::ContentBlock::Text("hi".into())],
        reply_to: None,
        metadata: MessageMetadata::default(),
        ordinal: None,
    };
    channel.dispatch_agent(outgoing.into());

    assert_eq!(bucket_a.lock().len(), 1, "tab A received the dispatch");
    assert_eq!(bucket_b.lock().len(), 1, "tab B received the dispatch");

    // After detach the remaining subscriber still receives.
    channel.detach(id_b);
    channel.dispatch_agent(AgentOutput {
        session_id: "sess-shared".into(),
        user_id: "u1".into(),
        channel: ChannelType::owner(),
        event: AgentEvent::AnswerDelta("chunk".into()),
    });
    assert_eq!(bucket_a.lock().len(), 2);
    assert_eq!(bucket_b.lock().len(), 1, "detached tab no longer receives");

    // Emission to a session with zero subscribers is silently dropped.
    channel.dispatch_agent(AgentOutput {
        session_id: "nobody-here".into(),
        user_id: String::new(),
        channel: ChannelType::owner(),
        event: AgentEvent::Notice {
            level: baybo_channels::NoticeLevel::Info,
            text: "ignored".into(),
            mid_turn: false,
            durable_id: None,
        },
    });
    assert_eq!(
        bucket_a.lock().len(),
        2,
        "no extra deliveries for unrelated session"
    );
}

/// Pinning a **cron group** (`docs/cron-groups.md`) flips a bit on the JOB — the
/// group is a view over the job's fires, and the job is the only object whose
/// identity matches it. Every fire row then carries `cron_group_pinned`, which
/// the clients fold into the one group row exactly as they already do the title.
///
/// Deleting the job is a recycle bin (soft delete): the `pinned` column lives
/// on, but a deleted job drops out of the live job list, so its group renders
/// with the tombstone name and reads UNPINNED while in the bin (a restore would
/// bring the pin back). That is accepted semantics, and this pins it so nobody
/// "fixes" it into a stored row later.
#[tokio::test]
async fn a_cron_group_pin_rides_the_job_and_reads_unpinned_once_deleted() {
    use baybo_cron::NewCronJob;
    use baybo_model::{CronSchedule, TriggerSource};

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let operator = User {
        id: baybo_gateway::auth::WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::owner(),
    };
    // Deliberately UNTITLED: the list handler used to drop title-less jobs from
    // its map wholesale, which was harmless when the map only carried the title —
    // and would silently unpin this group now that it carries the pin too.
    let job = tg
        .deps
        .cron_scheduler
        .create_job(NewCronJob {
            user_id: operator.id.clone(),
            channel: ChannelType::owner(),
            title: String::new(),
            schedule: CronSchedule::Cron {
                expr: "0 8 * * 1".into(),
            },
            prompt: "weekly digest".into(),
            timezone: "UTC".into(),
            origin_session_id: None,
        })
        .await
        .expect("create cron job");

    let fire = tg
        .deps
        .session_manager
        .create_session_with_trigger(
            operator.clone(),
            ChannelType::owner(),
            TriggerSource::Cron {
                cron_job_id: job.id.clone(),
                origin_session_id: None,
                conversation: true,
                job_title: Some("Weekly digest".into()),
            },
        )
        .await
        .expect("create recurring fire");

    let router = build_router(build_admin_state(&tg));
    let row_for = |list: &Value, id: &str| -> Value {
        list["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|row| row["session_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("session {id} missing from the chat list"))
    };

    let row = row_for(
        &get(&router, "/v1/chat/sessions", StatusCode::OK).await,
        fire.id.as_str(),
    );
    assert_eq!(
        row["cron_group_pinned"], false,
        "a new group starts unpinned"
    );

    put(
        &router,
        &format!("/v1/cron/{}/pin", job.id),
        Body::from(json!({ "pinned": true }).to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;

    let row = row_for(
        &get(&router, "/v1/chat/sessions", StatusCode::OK).await,
        fire.id.as_str(),
    );
    assert_eq!(
        row["cron_group_pinned"], true,
        "the fire must carry its job's pin — an UNTITLED job's pin especially, \
         which the title map used to filter away",
    );
    assert_eq!(
        row["pinned"], false,
        "the SESSION is untouched: pinning the group is not pinning its fires",
    );

    // Soft-delete the job: the history stays grouped (tombstone), but the job
    // drops out of the live list, so its group reads unpinned while binned.
    tg.deps
        .cron_scheduler
        .delete_job(&job.id)
        .await
        .expect("delete cron job");

    let row = row_for(
        &get(&router, "/v1/chat/sessions", StatusCode::OK).await,
        fire.id.as_str(),
    );
    assert_eq!(
        row["cron_job_id"], job.id,
        "a deleted job must not un-group its history",
    );
    assert_eq!(
        row["cron_group_pinned"], false,
        "a binned job's group reads unpinned — it is filtered from the live list",
    );
}

/// The pin route is pool-scoped. The phone and the web tab are one owner pool,
/// so a device CAN pin a job created on the web (including legacy `http` jobs).
/// A job on a non-pool channel (`tui`) stays isolated — indistinguishable from
/// one that does not exist, as is a genuinely-missing id.
#[tokio::test]
async fn a_cron_pin_reaches_across_the_owner_pool_but_not_outside_it() {
    use baybo_cron::NewCronJob;
    use baybo_model::CronSchedule;

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install channels");

    let device_key = device_proto::delegation::generate_signing_key();
    let device_id = device_proto::delegation::device_id_for(&device_key.verifying_key());

    let mk = |channel: ChannelType, title: &str| NewCronJob {
        user_id: baybo_gateway::auth::OWNER_USER_ID.into(),
        channel,
        title: title.into(),
        schedule: CronSchedule::Cron {
            expr: "0 8 * * *".into(),
        },
        prompt: "brief me".into(),
        timezone: "UTC".into(),
        origin_session_id: None,
    };

    // A legacy `http` job (same owner pool as the phone) and a private `tui` job.
    let web_job = tg
        .deps
        .cron_scheduler
        .create_job(mk(ChannelType::owner(), "Morning brief"))
        .await
        .expect("create web job");
    let tui_job = tg
        .deps
        .cron_scheduler
        .create_job(mk(ChannelType::tui(), "Terminal task"))
        .await
        .expect("create tui job");

    let router = build_admin_router_for_tests(&tg.deps);

    // The phone CAN pin the web-pool job — unified owner pool.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/cron/{}/pin", web_job.id))
                .header("authorization", format!("Bearer {}", tg.deps.admin_token))
                .header(DEVICE_ID_HEADER, &device_id)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "pinned": true }).to_string()))
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "a device must be able to pin a web-pool job",
    );
    let after = tg
        .deps
        .cron_scheduler
        .get_job(&web_job.id)
        .await
        .expect("get job")
        .expect("job exists");
    assert!(after.pinned, "the cross-surface pin must have landed");

    // The phone must NOT reach the `tui` job — indistinguishable from missing.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/cron/{}/pin", tui_job.id))
                .header("authorization", format!("Bearer {}", tg.deps.admin_token))
                .header(DEVICE_ID_HEADER, &device_id)
                .header("content-type", "application/json")
                .body(Body::from(json!({ "pinned": true }).to_string()))
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a device must not pin a non-pool tui job",
    );
    let after = tg
        .deps
        .cron_scheduler
        .get_job(&tui_job.id)
        .await
        .expect("get job")
        .expect("job exists");
    assert!(!after.pinned, "the rejected request must not have written");

    // A missing id answers the same way.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/cron/nope/pin")
                .header("authorization", format!("Bearer {}", tg.deps.admin_token))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "pinned": true }).to_string()))
                .unwrap(),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The point of the whole feature, over HTTP: a two-character Chinese word is
/// findable mid-run, which bare `unicode61` cannot do (the entire Han run is one
/// token, so `MATCH '迁移'` misses `数据库的迁移`). See `docs/search.md`.
#[tokio::test]
async fn chat_search_finds_a_chinese_word_inside_a_han_run() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let sid = SessionId::from(cred["session_id"].as_str().expect("session_id").to_owned());
    for msg in [
        ChatMessage::user(vec![ContentBlock::Text("数据库的迁移怎么做".into())]),
        ChatMessage::assistant(vec![ContentBlock::Text("先看 schema 再说".into())]),
    ] {
        tg.deps
            .session_manager
            .append_session_message(&sid, &msg)
            .await
            .expect("append");
    }

    let res = get(
        &router,
        "/v1/chat/search?q=%E8%BF%81%E7%A7%BB",
        StatusCode::OK,
    )
    .await;
    let groups = res["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["session_id"].as_str(), Some(sid.as_str()));
    assert_eq!(groups[0]["total_hits"].as_u64(), Some(1));
    let hit = &groups[0]["hits"][0];
    assert_eq!(hit["role"].as_str(), Some("user"));
    assert_eq!(hit["text"].as_str(), Some("数据库的迁移怎么做"));
    assert_eq!(hit["ordinal"].as_i64(), Some(0));
    assert_eq!(res["truncated"].as_bool(), Some(false));

    // Latin keeps word semantics, and the store widens it to reach suffixes.
    let res = get(&router, "/v1/chat/search?q=schema", StatusCode::OK).await;
    assert_eq!(
        res["groups"][0]["hits"][0]["role"].as_str(),
        Some("assistant")
    );

    // A query with nothing indexable is a user typing, not a 500.
    let empty = get(&router, "/v1/chat/search?q=---", StatusCode::OK).await;
    assert!(empty["groups"].as_array().expect("groups").is_empty());

    // FTS5 operators are inert: the store quotes the whole query literally.
    for hostile in ["q=%22+OR+%22", "q=NEAR%28a+b%29", "q=-x", "q=%28%28%28"] {
        get(
            &router,
            &format!("/v1/chat/search?{hostile}"),
            StatusCode::OK,
        )
        .await;
    }
}

/// Grouping is what keeps one chatty conversation from eating the whole result
/// set. Measured on real data, `codex` matches 47 times across 17 conversations
/// while a flat top-30 covers only 7 — one conversation takes 15 of the 30
/// slots. A flat list cannot be fixed client-side: the others were never sent.
#[tokio::test]
async fn chat_search_groups_by_conversation_so_one_cannot_crowd_out_the_rest() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    // One conversation says it many times; two others say it once.
    let mut ids = Vec::new();
    for n in [12usize, 1, 1] {
        let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
        let sid = SessionId::from(cred["session_id"].as_str().expect("session_id").to_owned());
        for i in 0..n {
            tg.deps
                .session_manager
                .append_session_message(
                    &sid,
                    &ChatMessage::user(vec![ContentBlock::Text(format!("检索 第{i}条"))]),
                )
                .await
                .expect("append");
        }
        ids.push(sid);
    }

    let res = get(
        &router,
        "/v1/chat/search?q=%E6%A3%80%E7%B4%A2",
        StatusCode::OK,
    )
    .await;
    let groups = res["groups"].as_array().expect("groups");
    assert_eq!(
        groups.len(),
        3,
        "every conversation must appear, not just the loud one"
    );

    let loud = groups
        .iter()
        .find(|g| g["session_id"].as_str() == Some(ids[0].as_str()))
        .expect("the loud conversation");
    assert_eq!(loud["total_hits"].as_u64(), Some(12), "counts every match");
    assert_eq!(
        loud["hits"].as_array().unwrap().len(),
        3,
        "but carries only the best few excerpts",
    );
}

/// "Find it in *this* chat" — and the filter composes with the others.
#[tokio::test]
async fn chat_search_scopes_to_one_session() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let mut ids = Vec::new();
    for text in ["第一个会话的检索", "第二个会话的检索"] {
        let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
        let sid = SessionId::from(cred["session_id"].as_str().expect("session_id").to_owned());
        tg.deps
            .session_manager
            .append_session_message(
                &sid,
                &ChatMessage::user(vec![ContentBlock::Text(text.into())]),
            )
            .await
            .expect("append");
        ids.push(sid);
    }

    let q = "/v1/chat/search?q=%E6%A3%80%E7%B4%A2";
    assert_eq!(
        get(&router, q, StatusCode::OK).await["groups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let scoped = get(
        &router,
        &format!("{q}&session_id={}", ids[0].as_str()),
        StatusCode::OK,
    )
    .await;
    let groups = scoped["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["session_id"].as_str(), Some(ids[0].as_str()));
}

/// `hidden` is the user saying "remove this from my list"; search must honour it
/// by default, and say so only when explicitly asked.
#[tokio::test]
async fn chat_search_respects_the_hidden_flag() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_id = cred["session_id"].as_str().expect("session_id").to_owned();
    tg.deps
        .session_manager
        .append_session_message(
            &SessionId::from(session_id.clone()),
            &ChatMessage::user(vec![ContentBlock::Text("被隐藏的检索内容".into())]),
        )
        .await
        .expect("append");

    let q = "/v1/chat/search?q=%E6%A3%80%E7%B4%A2";
    assert_eq!(
        get(&router, q, StatusCode::OK).await["groups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let del = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/chat/sessions/{session_id}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(del).await.expect("router responds");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(
        get(&router, q, StatusCode::OK).await["groups"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a hidden session must drop out of the default search scope"
    );
    assert_eq!(
        get(&router, &format!("{q}&include_hidden=true"), StatusCode::OK).await["groups"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "include_hidden must still reach it"
    );
}
