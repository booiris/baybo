//! Integration coverage for the admin-side `/v1/chat/*` REST surface.
//!
//! Spins a tower-style admin router (no TCP listener) and walks the
//! happy path: create session → mint credential → list shows it →
//! get returns transcript → refresh issues a fresh token (the old
//! one stays live so concurrent tabs don't revoke each other) →
//! DELETE hides the row (the session itself stays on the server,
//! only the chat list filters it) → unhide restores it to the
//! default listing.

use std::sync::Arc;

use aura_channels::ChannelKind;
use aura_config::ChannelsConfig;
use aura_gateway::auth::WEB_CLIENT_LABEL_PREFIX;
use aura_gateway::channel::boot;
use aura_gateway::test_support::build_test_deps;
use aura_model::{ChatMessage, ContentBlock, Role, SessionId};
use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
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
    let token = cred["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_owned();
    assert!(!token.is_empty(), "minted token should be non-empty");
    assert!(
        cred["channel_token_header"].is_string(),
        "credential carries the header name",
    );

    // The token is alive in the live channel_tokens table.
    let identity = state
        .channel_tokens
        .lookup(&token)
        .expect("token must be live in the table");
    assert!(
        identity.label.starts_with(WEB_CLIENT_LABEL_PREFIX),
        "label is web/<uuid>: got {}",
        identity.label,
    );
    assert_eq!(identity.bound_channel_type.as_deref(), Some("http"));

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

    // ── 4. Refresh issues a fresh token, leaves the old one live ────
    // Two tabs against the same anchor session must each keep their
    // own working token — keying the handle map by token (not
    // session_id) is what prevents the second mint from revoking the
    // first tab's bearer.
    let refreshed = post(
        &router,
        &format!("/v1/chat/sessions/{session_id}/token"),
        Body::empty(),
        StatusCode::OK,
    )
    .await;
    let new_token = refreshed["channel_token"]
        .as_str()
        .expect("refreshed token");
    assert_ne!(new_token, token, "refresh must mint a fresh token");
    assert!(
        state.channel_tokens.lookup(&token).is_some(),
        "old token must stay live after refresh so the sibling tab's WS keeps working",
    );
    assert!(
        state.channel_tokens.lookup(new_token).is_some(),
        "new token must be live after refresh",
    );

    // ── 5. Slash manifest exposes /compact but hides /new ───────────
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

    // ── 6. DELETE hides — row + token both stay live ────────────────
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
    assert!(
        state.channel_tokens.lookup(new_token).is_some(),
        "token must still be live — hide is a soft filter, not a revocation",
    );

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

    // ── 7. Unhide brings it back into the default list ──────────────
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

// ── helpers ─────────────────────────────────────────────────────────

fn build_admin_state(
    tg: &aura_gateway::test_support::TestGateway,
) -> aura_gateway::server::AdminState {
    aura_gateway::server::AdminState {
        config: Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: Arc::clone(&tg.deps.memory_manager),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        query_api: Arc::new(aura_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.job_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: Arc::clone(&tg.deps.skill_registry),
        tool_registry: Arc::clone(&tg.deps.tool_registry),
        channel_registry: Arc::clone(&tg.deps.channel_registry),
        llm_client: tg.deps.llm_client.clone(),
        log_buffer: Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        channel_control: Arc::clone(&tg.deps.channel_control),
        secret_vault: Arc::clone(&tg.deps.secret_vault),
        channel_tokens: tg.deps.channel_tokens.clone(),
        web_chat_tokens: Arc::clone(&tg.deps.web_chat_tokens),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    }
}

fn build_router(state: aura_gateway::server::AdminState) -> axum::Router {
    let (router, _spec) = aura_gateway::api::admin::v1_router_and_spec();
    router.with_state(state)
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
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("first ask".into())],
            from_user: true,
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("first reply".into())],
            from_user: false,
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("second\nask\nwith   newlines".into())],
            from_user: true,
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("second reply".into())],
            from_user: false,
        },
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
    use aura_model::{ChannelType, TriggerSource, User};

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
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("[cron:cj-test] morning brief".into())],
            from_user: true,
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("daily summary\nready".into())],
            from_user: false,
        },
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
    let reg = Arc::new(aura_channels::ChannelRegistry::new());
    let cfg = ChannelsConfig::default();
    boot::install_channels(&reg, &cfg).expect("install");
    let ch = reg
        .get(&aura_model::ChannelType::http())
        .expect("http channel exists");
    assert_eq!(ch.kind(), ChannelKind::Subscribed);
}

// Bonus: multi-attach + fan-out smoke test directly against the
// Channel API (no WS), confirming the design invariant that the same
// `session_id` can have N connections and an emission reaches all of
// them.
#[tokio::test]
async fn channel_multi_attach_fans_out_to_all_subscribers() {
    use aura_channels::wire::Frame;
    use aura_channels::{
        AgentOutput, Channel, ChannelKind, Connection, ConnectionSink, OutgoingMessage,
        SendOutcome, SessionEvent,
    };
    use aura_model::{ChannelType, MessageMetadata};
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
        content: vec![aura_model::ContentBlock::Text("hi".into())],
        reply_to: None,
        metadata: MessageMetadata::default(),
        ordinal: None,
    };
    channel.dispatch_agent(AgentOutput::Message(outgoing));

    assert_eq!(bucket_a.lock().len(), 1, "tab A received the dispatch");
    assert_eq!(bucket_b.lock().len(), 1, "tab B received the dispatch");

    // After detach the remaining subscriber still receives.
    channel.detach(id_b);
    channel.dispatch_agent(AgentOutput::Delta {
        session_id: "sess-shared".into(),
        user_id: "u1".into(),
        channel: ChannelType::http(),
        text: "chunk".into(),
    });
    assert_eq!(bucket_a.lock().len(), 2);
    assert_eq!(bucket_b.lock().len(), 1, "detached tab no longer receives");

    // Emission to a session with zero subscribers is silently dropped.
    channel.dispatch_agent(AgentOutput::Notice {
        session_id: "nobody-here".into(),
        user_id: String::new(),
        channel: ChannelType::http(),
        level: aura_channels::NoticeLevel::Info,
        text: "ignored".into(),
    });
    assert_eq!(
        bucket_a.lock().len(),
        2,
        "no extra deliveries for unrelated session"
    );
}
