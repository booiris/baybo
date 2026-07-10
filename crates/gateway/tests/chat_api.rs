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
use baybo_model::{AgentProfileId, ChannelType, ChatMessage, ContentBlock, SessionId, User};
use baybo_store::{DeviceRow, DeviceStatus};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn chat_api_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
async fn chat_create_accepts_client_supplied_session_id() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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

#[tokio::test]
async fn create_session_binds_agent_profile() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    // ── 1. Create a baybo-framework profile ──────────────────────────
    let profile = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "helper" }).to_string()),
        StatusCode::OK,
    )
    .await;
    let agent_id = profile["id"].as_str().expect("profile id").to_owned();

    // ── 2. Create a session bound to it ───────────────────────────────
    let created = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": agent_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    let session_id = created["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    // ── 3. Detail carries the binding ─────────────────────────────────
    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{session_id}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["agent_id"].as_str(), Some(agent_id.as_str()));
    assert_eq!(detail["agent_framework"].as_str(), Some("baybo"));

    // ── 4. List row carries the binding too ───────────────────────────
    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    let row = items
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id.as_str()))
        .expect("created session shows up in the list");
    assert_eq!(row["agent_id"].as_str(), Some(agent_id.as_str()));

    // ── 5. The builtin id normalizes to unbound ───────────────────────
    let builtin_created = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": "baybo" }).to_string()),
        StatusCode::OK,
    )
    .await;
    let builtin_session_id = builtin_created["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let builtin_detail = get(
        &router,
        &format!("/v1/chat/sessions/{builtin_session_id}"),
        StatusCode::OK,
    )
    .await;
    assert!(
        builtin_detail.get("agent_id").is_none(),
        "builtin agent_id must normalize to unbound, got {builtin_detail:?}",
    );
    assert!(
        builtin_detail.get("agent_framework").is_none(),
        "an unbound session must not carry agent_framework, got {builtin_detail:?}",
    );

    // ── 6. Unknown agent_id is a 400 ──────────────────────────────────
    post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": "nope" }).to_string()),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 7. Non-baybo framework is a 400 mentioning "not supported yet" ─
    let ext_profile = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "ext", "framework": "claude" }).to_string()),
        StatusCode::OK,
    )
    .await;
    let ext_agent_id = ext_profile["id"].as_str().expect("profile id").to_owned();
    let err = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": ext_agent_id }).to_string()),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("not supported yet"),
        "expected 'not supported yet' in error, got {err:?}",
    );

    // ── 8a. Idempotent retry: same session_id + the SAME agent_id it's
    // already bound to → 200, returns the existing session unchanged
    // (a client safely resending its create call after e.g. a dropped
    // response must not see a 400).
    let retried = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": session_id, "agent_id": agent_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    assert_eq!(retried["session_id"].as_str(), Some(session_id.as_str()));

    // ── 8b. Mismatch: an agent_id against an already-*unbound* existing
    // session is a 400 ─────────────────────────────────────────────────
    post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": builtin_session_id, "agent_id": agent_id }).to_string()),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 8c. Mismatch: the same session_id + a DIFFERENT agent_id than
    // it's bound to is a 400 ────────────────────────────────────────────
    let profile2 = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "helper2" }).to_string()),
        StatusCode::OK,
    )
    .await;
    let agent_id2 = profile2["id"].as_str().expect("profile id").to_owned();
    post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": session_id, "agent_id": agent_id2 }).to_string()),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 9. Client-supplied FRESH session_id + agent_id binds too ──────
    // (exercises the requested-id fresh-create arm's stamping tail).
    let requested_sid = "client-agent-session-1";
    let created = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "session_id": requested_sid, "agent_id": agent_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    assert_eq!(created["session_id"].as_str(), Some(requested_sid));
    let detail = get(
        &router,
        &format!("/v1/chat/sessions/{requested_sid}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["agent_id"].as_str(), Some(agent_id.as_str()));
    assert_eq!(detail["agent_framework"].as_str(), Some("baybo"));
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

#[tokio::test]
async fn chat_sync_difference_is_full_fidelity_with_coverage_watermark() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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

#[tokio::test]
async fn chat_list_uses_device_scope_when_forwarded_from_tunnel() {
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
                channel: ChannelType::device(),
            },
            ChannelType::device(),
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
                channel: ChannelType::http(),
            },
            ChannelType::http(),
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
    assert_eq!(ids, vec![device_session.id.as_str()]);
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
    assert_eq!(session.channel, ChannelType::device());
    assert_eq!(session.user.id, device_id);
    assert_eq!(session.user.channel, ChannelType::device());

    let list = authed_device_request(
        &router,
        "GET",
        "/v1/chat/sessions",
        &tg.deps.admin_token,
        &session.user.id,
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
        &session.user.id,
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
    assert_eq!(
        web_response.status(),
        StatusCode::NOT_FOUND,
        "plain web identity must not see device-scoped sessions",
    );
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
    assert_eq!(session.channel, ChannelType::device());
    assert_eq!(session.user.id, device_id);
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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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

// Per-session model switch respects the bound agent's `allowed_models`
// set: a pin outside the set is a 400, clearing the pin always bypasses
// the check, an unbound session has no restriction, and a profile
// deleted after the session bound to it is tolerated (skip, not 500).
#[tokio::test]
async fn set_session_model_enforces_agent_allowed_set() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

    let mut state = build_admin_state(&tg);
    let (llm_pool, entry_a, entry_b) = baybo_gateway::test_support::two_entry_llm_pool(&tg);
    state.llm_pool = llm_pool;
    let router = build_router(state);

    // ── 1. Session bound to a profile whose set contains the pin → 200,
    // and switching to a name outside the set is a 400 ─────────────────
    let profile_a = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "Restricted A", "allowed_models": [entry_a] }).to_string()),
        StatusCode::OK,
    )
    .await;
    let agent_a_id = profile_a["id"].as_str().expect("id").to_owned();
    let session_a = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": agent_a_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    let session_a_id = session_a["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let model_uri_a = format!("/v1/chat/sessions/{session_a_id}/model");
    let set = put(
        &router,
        &model_uri_a,
        Body::from(format!(r#"{{"llm":"{entry_a}"}}"#)),
        StatusCode::OK,
    )
    .await;
    assert_eq!(set["last_llm"].as_str(), Some(entry_a.as_str()));

    let err = put(
        &router,
        &model_uri_a,
        Body::from(format!(r#"{{"llm":"{entry_b}"}}"#)),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"].as_str().unwrap_or("").contains("allowed set"),
        "expected allowed-set error, got {err:?}",
    );

    // ── 2. Clearing the pin always bypasses the set check ───────────────
    let profile_b = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "Restricted B", "allowed_models": [entry_a] }).to_string()),
        StatusCode::OK,
    )
    .await;
    let agent_b_id = profile_b["id"].as_str().expect("id").to_owned();
    let session_b = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": agent_b_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    let session_b_id = session_b["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    put(
        &router,
        &format!("/v1/chat/sessions/{session_b_id}/model"),
        Body::from(r#"{"llm":null}"#),
        StatusCode::OK,
    )
    .await;

    // ── 3. An unbound session has no restriction ─────────────────────────
    let session_c = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let session_c_id = session_c["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    put(
        &router,
        &format!("/v1/chat/sessions/{session_c_id}/model"),
        Body::from(format!(r#"{{"llm":"{entry_b}"}}"#)),
        StatusCode::OK,
    )
    .await;

    // ── 4. A profile deleted after the session bound to it is tolerated
    // (skip the check rather than 500) ────────────────────────────────────
    let profile_d = post(
        &router,
        "/v1/agents",
        Body::from(json!({ "name": "Restricted D", "allowed_models": [entry_a] }).to_string()),
        StatusCode::OK,
    )
    .await;
    let agent_d_id = profile_d["id"].as_str().expect("id").to_owned();
    let session_d = post(
        &router,
        "/v1/chat/sessions",
        Body::from(json!({ "agent_id": agent_d_id }).to_string()),
        StatusCode::OK,
    )
    .await;
    let session_d_id = session_d["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    tg.deps
        .stores
        .agent_profile
        .delete(&AgentProfileId::from(agent_d_id.as_str()))
        .await
        .expect("delete agent profile");
    put(
        &router,
        &format!("/v1/chat/sessions/{session_d_id}/model"),
        Body::from(format!(r#"{{"llm":"{entry_b}"}}"#)),
        StatusCode::OK,
    )
    .await;
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
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

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

// Cron-spawned sessions are filtered out of `GET /v1/chat/sessions` so
// the operator's chat sidebar isn't buried under background fires;
// they instead surface via `GET /v1/chat/cron-messages`. The opt-in
// `?include_cron=true` query restores them for parity with
// `include_hidden`.
#[tokio::test]
async fn cron_sessions_split_into_dedicated_endpoint() {
    use baybo_model::{ChannelType, TriggerSource, User};

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let http_config = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &http_config).expect("install http channel");

    let state = build_admin_state(&tg);
    let router = build_router(state.clone());

    let user_cred = post(&router, "/v1/chat/sessions", Body::empty(), StatusCode::OK).await;
    let user_id = user_cred["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    let cron_session = tg
        .deps
        .session_manager
        .create_session_with_trigger(
            User {
                id: "operator".into(),
                name: None,
                channel: ChannelType::http(),
            },
            ChannelType::http(),
            TriggerSource::Cron {
                cron_job_id: "cj-test".into(),
            },
        )
        .await
        .expect("create cron session");
    let cron_id = cron_session.id.to_string();

    let cron_rows: &[ChatMessage] = &[
        // The cron prompt persists as a `MessageSource::Cron` row; the inbox
        // locates it by that provenance, then strips the `[cron:<id>]` framing
        // for display.
        ChatMessage::cron_fire(vec![ContentBlock::Text(
            "[cron:cj-test] morning brief".into(),
        )]),
        ChatMessage::assistant(vec![ContentBlock::Text("daily summary\nready".into())]),
    ];
    for msg in cron_rows {
        tg.deps
            .session_manager
            .append_session_message(&cron_session.id, msg)
            .await
            .expect("append cron row");
    }

    let list = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|row| row["session_id"].as_str() == Some(user_id.as_str())),
        "user session must show up in default list",
    );
    assert!(
        items
            .iter()
            .all(|row| row["session_id"].as_str() != Some(cron_id.as_str())),
        "cron session must be hidden from default chat list, got {items:?}",
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
            .any(|row| row["session_id"].as_str() == Some(cron_id.as_str())),
        "include_cron=true must bring cron sessions back",
    );

    let inbox = get(&router, "/v1/chat/cron-messages", StatusCode::OK).await;
    let inbox_items = inbox["items"].as_array().expect("items");
    let cron_row = inbox_items
        .iter()
        .find(|row| row["session_id"].as_str() == Some(cron_id.as_str()))
        .expect("cron inbox should surface the cron session");
    assert_eq!(cron_row["cron_job_id"].as_str(), Some("cj-test"));
    assert_eq!(
        cron_row["prompt"].as_str(),
        Some("morning brief"),
        "prompt must strip the `[cron:<id>] ` routing prefix",
    );
    assert_eq!(
        cron_row["response"].as_str(),
        Some("daily summary ready"),
        "response must collapse to a single-line preview",
    );
    assert!(
        inbox_items
            .iter()
            .all(|row| row["session_id"].as_str() != Some(user_id.as_str())),
        "cron inbox must not contain user-triggered sessions",
    );
}

// Channel kind sanity: every kind the boot path declares must match
// the protocol invariant that http is `Subscribed`.
#[test]
fn http_channel_kind_is_subscribed() {
    let reg = Arc::new(baybo_channels::ChannelRegistry::new());
    let cfg = ChannelsConfig::default();
    boot::install_channels(&reg, &cfg).expect("install");
    let ch = reg
        .get(&baybo_model::ChannelType::http())
        .expect("http channel exists");
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
        ChannelType::http(),
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
        channel: ChannelType::http(),
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
        channel: ChannelType::http(),
        event: AgentEvent::AnswerDelta("chunk".into()),
    });
    assert_eq!(bucket_a.lock().len(), 2);
    assert_eq!(bucket_b.lock().len(), 1, "detached tab no longer receives");

    // Emission to a session with zero subscribers is silently dropped.
    channel.dispatch_agent(AgentOutput {
        session_id: "nobody-here".into(),
        user_id: String::new(),
        channel: ChannelType::http(),
        event: AgentEvent::Notice {
            level: baybo_channels::NoticeLevel::Info,
            text: "ignored".into(),
        },
    });
    assert_eq!(
        bucket_a.lock().len(),
        2,
        "no extra deliveries for unrelated session"
    );
}
