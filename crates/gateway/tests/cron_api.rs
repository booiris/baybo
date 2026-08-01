//! Integration coverage for the admin-side `/v1/cron` REST surface:
//! pause / resume, the recycle bin (soft delete → listed only under
//! `?deleted=true`, still resolvable by id, restorable), and the in-place
//! edit.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_gateway::test_support::build_test_deps;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn cron_pause_resume_and_recycle_bin_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_router(build_admin_state(&tg));

    // ── 1. Create: enabled, scheduled, live ─────────────────────────
    let created = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "0 9 * * *",
            "user_id": "owner",
            "title": "Morning digest",
            "text": "Summarize the news",
            "timezone": "UTC",
        }),
        StatusCode::CREATED,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["status"].as_str(), Some("enabled"));
    assert!(!created["next_trigger_at"].is_null(), "{created:?}");
    assert!(
        created.get("deleted_at").is_none(),
        "a live job carries no deleted_at: {created:?}",
    );

    let bad = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "not a cron expression",
            "user_id": "owner",
            "title": "Broken",
            "text": "…",
            "timezone": "UTC",
        }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid schedule"),
        "{bad:?}",
    );

    // ── 2. Pause clears the next trigger; resume recomputes it ──────
    post_expect(
        &router,
        &format!("/v1/cron/{id}/pause"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let paused = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert_eq!(paused["status"].as_str(), Some("disabled"));
    assert!(
        paused["next_trigger_at"].is_null(),
        "a paused job has no next trigger: {paused:?}",
    );
    assert_eq!(
        listed_ids(&router, false).await,
        vec![id.clone()],
        "pausing keeps the job in the live list",
    );

    post_expect(
        &router,
        &format!("/v1/cron/{id}/resume"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let resumed = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert_eq!(resumed["status"].as_str(), Some("enabled"));
    assert!(!resumed["next_trigger_at"].is_null(), "{resumed:?}");

    // ── 3. Delete is a soft delete: out of the live list, into the bin,
    //       still resolvable by id ────────────────────────────────────
    delete_expect(&router, &format!("/v1/cron/{id}"), StatusCode::NO_CONTENT).await;
    assert!(
        listed_ids(&router, false).await.is_empty(),
        "the default list never carries a deleted job",
    );
    assert_eq!(listed_ids(&router, true).await, vec![id.clone()]);
    let deleted = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert!(
        !deleted["deleted_at"].is_null(),
        "a binned job reports when it was deleted: {deleted:?}",
    );
    assert_eq!(
        deleted["status"].as_str(),
        Some("enabled"),
        "deletion is orthogonal to status",
    );

    // ── 4. Pause / resume do not act on a job in the bin: reporting
    //       success would promise a fire `list_due` can never produce ─
    for action in ["pause", "resume"] {
        post_expect(
            &router,
            &format!("/v1/cron/{id}/{action}"),
            json!({}),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    // ── 5. Restore brings it back, live and scheduled from now ──────
    post_expect(
        &router,
        &format!("/v1/cron/{id}/restore"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(listed_ids(&router, false).await, vec![id.clone()]);
    assert!(listed_ids(&router, true).await.is_empty());
    let restored = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert!(
        restored.get("deleted_at").is_none(),
        "restore clears deleted_at: {restored:?}",
    );
    assert_eq!(restored["status"].as_str(), Some("enabled"));
    assert!(!restored["next_trigger_at"].is_null(), "{restored:?}");

    // ── 6. Unknown ids 404 across the whole surface ─────────────────
    for action in ["pause", "resume", "restore"] {
        post_expect(
            &router,
            &format!("/v1/cron/missing/{action}"),
            json!({}),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    delete_expect(&router, "/v1/cron/missing", StatusCode::NOT_FOUND).await;
    get(&router, "/v1/cron/missing", StatusCode::NOT_FOUND).await;
}

/// `?channel=` behind the phone's cron job list. The route's DEFAULT is the
/// unfiltered operator view the admin dashboard renders — every channel in one
/// table — so the filter must be opt-in, and must not quietly become the default
/// for the callers that never ask.
#[tokio::test]
async fn listing_cron_can_be_filtered_to_one_channel() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_router(build_admin_state(&tg));

    let mut ids = std::collections::HashMap::new();
    for channel in ["owner", "telegram"] {
        let created = post_expect(
            &router,
            "/v1/cron",
            json!({
                "schedule": "0 9 * * *",
                "user_id": "owner",
                "channel": channel,
                "title": format!("{channel} digest"),
                "text": "Summarize the news",
                "timezone": "UTC",
            }),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(created["channel"].as_str(), Some(channel), "{created:?}");
        ids.insert(channel, created["id"].as_str().expect("id").to_owned());
    }

    let channels_of = |list: &Value| -> Vec<String> {
        list["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|job| job["channel"].as_str().map(str::to_owned))
            .collect()
    };

    // No param: the operator view. Both jobs, untouched by this change.
    let all = get(&router, "/v1/cron", StatusCode::OK).await;
    let seen = channels_of(&all);
    assert!(
        seen.contains(&"owner".to_string()) && seen.contains(&"telegram".to_string()),
        "the unfiltered list must stay the every-channel operator view, got {seen:?}",
    );

    // `?channel=owner`: only what a chat client can actually open.
    let owned = get(&router, "/v1/cron?channel=owner", StatusCode::OK).await;
    assert_eq!(channels_of(&owned), vec!["owner".to_string()], "{owned:?}");
    assert_eq!(
        owned["items"][0]["id"].as_str(),
        Some(ids["owner"].as_str()),
    );

    // The filter composes with the recycle bin rather than being swallowed by
    // it: delete BOTH, and the bin still answers for one channel only.
    for id in ids.values() {
        request(
            &router,
            "DELETE",
            &format!("/v1/cron/{id}"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    let bin = get(
        &router,
        "/v1/cron?deleted=true&channel=owner",
        StatusCode::OK,
    )
    .await;
    assert_eq!(channels_of(&bin), vec!["owner".to_string()], "{bin:?}");
    // …and the live list is empty for that channel now, not merely unfiltered.
    let live = get(&router, "/v1/cron?channel=owner", StatusCode::OK).await;
    assert!(
        live["items"].as_array().expect("items").is_empty(),
        "a deleted job must not survive in the filtered live list: {live:?}",
    );
}

#[tokio::test]
async fn cron_in_place_edit_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_router(build_admin_state(&tg));

    let created = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "0 9 * * *",
            "user_id": "owner",
            "title": "Morning digest",
            "text": "Summarize the news",
            "timezone": "UTC",
        }),
        StatusCode::CREATED,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let uri = format!("/v1/cron/{id}");
    let armed_at = trigger_at(&created);

    // ── 1. A patch writes what it carries and nothing else ──────────
    let edited = patch_expect(
        &router,
        &uri,
        json!({ "prompt": "Summarize the news, briefly" }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        edited["prompt"].as_str(),
        Some("Summarize the news, briefly")
    );
    assert_eq!(
        edited["title"].as_str(),
        Some("Morning digest"),
        "a field the patch left out keeps its value: {edited:?}",
    );
    assert_eq!(edited["schedule"]["expr"].as_str(), Some("0 9 * * *"));
    assert_eq!(
        trigger_at(&edited),
        armed_at,
        "editing the prompt does not move the fire time: {edited:?}",
    );
    assert_eq!(
        edited["id"].as_str(),
        Some(id.as_str()),
        "an edit keeps the job's id — that is the whole point of it over delete + recreate",
    );

    // ── 2. Rescheduling re-arms from now, never into the past ───────
    let rescheduled = patch_expect(
        &router,
        &uri,
        json!({ "schedule": { "kind": "cron", "expr": "30 6 * * *" } }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(rescheduled["schedule"]["expr"].as_str(), Some("30 6 * * *"));
    assert_eq!(rescheduled["status"].as_str(), Some("enabled"));
    let next = trigger_at(&rescheduled);
    assert!(
        next > Some(Utc::now()),
        "the new slot is ahead of now, not a missed one back-filled: {rescheduled:?}",
    );
    assert_ne!(next, armed_at, "{rescheduled:?}");
    assert_eq!(
        rescheduled["prompt"].as_str(),
        Some("Summarize the news, briefly"),
        "a reschedule keeps the prompt the last edit wrote: {rescheduled:?}",
    );

    // ── 3. A paused job stays paused: an edit is not a resume ───────
    post_expect(
        &router,
        &format!("{uri}/pause"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let paused_edit = patch_expect(
        &router,
        &uri,
        json!({
            "title": "Evening digest",
            "schedule": { "kind": "cron", "expr": "0 18 * * *" },
        }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(paused_edit["title"].as_str(), Some("Evening digest"));
    assert_eq!(paused_edit["schedule"]["expr"].as_str(), Some("0 18 * * *"));
    assert_eq!(
        paused_edit["status"].as_str(),
        Some("disabled"),
        "editing a paused job must not quietly restart it: {paused_edit:?}",
    );
    assert!(
        paused_edit["next_trigger_at"].is_null(),
        "a paused job holds no slot, edited or not: {paused_edit:?}",
    );

    post_expect(
        &router,
        &format!("{uri}/resume"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;

    // ── 4. The bodies a caller gets wrong ───────────────────────────
    let empty = patch_expect(&router, &uri, json!({}), StatusCode::BAD_REQUEST).await;
    assert!(
        empty["error"]
            .as_str()
            .unwrap_or("")
            .contains("sets no fields"),
        "{empty:?}",
    );

    let elapsed = patch_expect(
        &router,
        &uri,
        json!({ "schedule": { "kind": "at", "time": "2020-01-01T00:00:00Z" } }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        elapsed["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid schedule"),
        "{elapsed:?}",
    );
    let untouched = get(&router, &uri, StatusCode::OK).await;
    assert_eq!(
        untouched["schedule"]["expr"].as_str(),
        Some("0 18 * * *"),
        "a refused edit leaves the job exactly as it was: {untouched:?}",
    );
    assert_eq!(untouched["status"].as_str(), Some("enabled"));

    // ── 5. A job in the bin reads as absent: restore it first ───────
    delete_expect(&router, &uri, StatusCode::NO_CONTENT).await;
    patch_expect(
        &router,
        &uri,
        json!({
            "prompt": "…",
            "schedule": { "kind": "cron", "expr": "0 3 * * *" },
        }),
        StatusCode::NOT_FOUND,
    )
    .await;
    // The refusal is total: the row is still binned, and still holds every value
    // the edit tried to write over. A 404 that had half-applied the patch — or
    // that had walked the job back out of the bin to do it — would be worse than
    // no edit at all.
    let binned = get(&router, &uri, StatusCode::OK).await;
    assert!(
        !binned["deleted_at"].is_null(),
        "a refused edit brought the job back out of the bin: {binned:?}",
    );
    assert_eq!(
        binned["prompt"].as_str(),
        Some("Summarize the news, briefly"),
        "a job in the bin was edited: {binned:?}",
    );
    assert_eq!(binned["schedule"]["expr"].as_str(), Some("0 18 * * *"));
    assert!(listed_ids(&router, false).await.is_empty());
    assert_eq!(listed_ids(&router, true).await, vec![id.clone()]);

    // Restored, it takes the edit it refused while it was in the bin.
    post_expect(
        &router,
        &format!("{uri}/restore"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let edited = patch_expect(&router, &uri, json!({ "prompt": "…" }), StatusCode::OK).await;
    assert_eq!(edited["prompt"].as_str(), Some("…"));

    patch_expect(
        &router,
        "/v1/cron/missing",
        json!({ "prompt": "…" }),
        StatusCode::NOT_FOUND,
    )
    .await;
}

/// The API client is the one caller that can still hand a job an empty
/// instruction: the LLM tools filter a blank field out before it gets here, and
/// the web form refuses to submit one. A blank prompt is not a job that does
/// nothing — it is an armed job firing nothing on every slot.
#[tokio::test]
async fn a_blank_prompt_is_refused_on_create_and_on_edit() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_router(build_admin_state(&tg));

    for blank in ["", "   "] {
        post_expect(
            &router,
            "/v1/cron",
            json!({
                "schedule": "0 9 * * *",
                "user_id": "owner",
                "title": "Morning digest",
                "text": blank,
                "timezone": "UTC",
            }),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    let created = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "0 9 * * *",
            "user_id": "owner",
            "title": "Morning digest",
            "text": "Summarize the news",
            "timezone": "UTC",
        }),
        StatusCode::CREATED,
    )
    .await;
    let uri = format!("/v1/cron/{}", created["id"].as_str().expect("id"));

    for blank in ["", "   "] {
        patch_expect(
            &router,
            &uri,
            json!({ "prompt": blank }),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    let unchanged = get(&router, &uri, StatusCode::OK).await;
    assert_eq!(unchanged["prompt"].as_str(), Some("Summarize the news"));
    assert_eq!(trigger_at(&unchanged), trigger_at(&created));
}

// ── helpers ─────────────────────────────────────────────────────────

fn trigger_at(job: &Value) -> Option<DateTime<Utc>> {
    job["next_trigger_at"].as_str().map(|s| {
        DateTime::parse_from_rfc3339(s)
            .expect("rfc3339 trigger")
            .to_utc()
    })
}

async fn listed_ids(router: &axum::Router, deleted: bool) -> Vec<String> {
    let uri = if deleted {
        "/v1/cron?deleted=true"
    } else {
        "/v1/cron"
    };
    get(router, uri, StatusCode::OK).await["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|j| j["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn build_admin_state(
    tg: &baybo_gateway::test_support::TestGateway,
) -> baybo_gateway::server::AdminState {
    baybo_gateway::server::AdminState {
        // Per-test workspace, from the same tempdir the deps were built
        // with: the agents surface writes identity files under it, so a
        // shared path would leak one test's persona into the next.
        workspace_paths: std::sync::Arc::clone(&tg.deps.workspace_paths),
        config: Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: Arc::clone(&tg.deps.session_manager),
        turn_lifecycle: Arc::clone(&tg.deps.turn_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        message_search: tg.deps.stores.message_search.clone(),
        query_api: Arc::new(baybo_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.turn_lifecycle),
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
        deck_manager: Arc::clone(&tg.deps.deck_manager),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    }
}

fn build_router(state: baybo_gateway::server::AdminState) -> axum::Router {
    let (router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    router.with_state(state)
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
    };
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "{method} {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("body bytes");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response is json")
}

async fn get(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "GET", uri, None, expected).await
}

async fn post_expect(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "POST", uri, Some(body), expected).await
}

async fn patch_expect(
    router: &axum::Router,
    uri: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    request(router, "PATCH", uri, Some(body), expected).await
}

async fn delete_expect(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "DELETE", uri, None, expected).await
}
