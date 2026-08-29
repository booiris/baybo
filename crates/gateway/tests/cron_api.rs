//! Integration coverage for the admin-side `/v1/cron` REST surface:
//! pause / resume, the recycle bin (soft delete → listed only under
//! `?deleted=true`, still resolvable by id, restorable), and the in-place
//! edit.

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};
use baybo_model::{McpTransportIdentity, TrustLevel};
use baybo_tools::mcp::McpToolMetadata;
use baybo_tools::{Tool, ToolContext, ToolManifest, ToolOutput};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::sync::Arc;
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

#[tokio::test]
async fn grantable_mcp_tool_listing_is_authenticated_live_typed_and_sorted() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let zeta_identity =
        register_mcp_tool(&tg, "zeta", "zeta/search", "search", "Search zeta", 0x22);
    let alpha_identity = register_mcp_tool(&tg, "alpha", "alpha/read", "read", "Read alpha", 0x11);
    let untrusted_identity = register_mcp_tool_with_trust(
        &tg,
        "unsafe",
        "unsafe/read",
        "read",
        "Read unsafe",
        0x33,
        TrustLevel::Untrusted,
    );
    let router = build_authenticated_router(&tg);

    get(&router, "/v1/cron/mcp-tools", StatusCode::UNAUTHORIZED).await;
    let listed = admin_get(&router, "/v1/cron/mcp-tools", StatusCode::OK).await;
    assert_eq!(
        listed["items"],
        json!([
            {
                "server": "alpha",
                "tool": "alpha/read",
                "upstream": "read",
                "description": "Read alpha",
                "transport_identity": alpha_identity.to_string(),
            },
            {
                "server": "zeta",
                "tool": "zeta/search",
                "upstream": "search",
                "description": "Search zeta",
                "transport_identity": zeta_identity.to_string(),
            },
        ])
    );

    tg.deps.tool_registry.unregister_for_source("alpha");
    let live = admin_get(&router, "/v1/cron/mcp-tools", StatusCode::OK).await;
    assert_eq!(live["items"].as_array().expect("items").len(), 1);
    assert_eq!(live["items"][0]["tool"].as_str(), Some("zeta/search"));

    let mut create = new_cron_body("Unsafe grant");
    create["mcp_tool_grants"] = json!([grant_json("unsafe/read", &untrusted_identity)]);
    admin_post_expect(&router, "/v1/cron", create, StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn cron_create_patch_replace_and_revoke_exact_mcp_grants() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let first_identity = register_mcp_tool(&tg, "alpha", "alpha/read", "read", "Read alpha", 0x11);
    let second_identity = register_mcp_tool(&tg, "beta", "beta/write", "write", "Write beta", 0x22);
    let router = build_authenticated_router(&tg);

    let default_empty = admin_post_expect(
        &router,
        "/v1/cron",
        new_cron_body("No grants"),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(default_empty["mcp_tool_grants"], json!([]));

    let first_grant = grant_json("alpha/read", &first_identity);
    let mut create = new_cron_body("Granted");
    create["mcp_tool_grants"] = json!([first_grant.clone()]);
    let created = admin_post_expect(&router, "/v1/cron", create, StatusCode::CREATED).await;
    assert_eq!(created["mcp_tool_grants"], json!([first_grant.clone()]));
    let uri = format!("/v1/cron/{}", created["id"].as_str().expect("id"));

    let prompt_only = admin_patch_expect(
        &router,
        &uri,
        json!({"prompt": "Updated without touching grants"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(prompt_only["mcp_tool_grants"], json!([first_grant]));

    let second_grant = grant_json("beta/write", &second_identity);
    let replaced = admin_patch_expect(
        &router,
        &uri,
        json!({
            "prompt": "Mixed authored and grant update",
            "mcp_tool_grants": [second_grant.clone()],
        }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(replaced["mcp_tool_grants"], json!([second_grant]));
    assert_eq!(
        replaced["prompt"].as_str(),
        Some("Mixed authored and grant update")
    );

    let revoked = admin_patch_expect(
        &router,
        &uri,
        json!({"mcp_tool_grants": []}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(revoked["mcp_tool_grants"], json!([]));
}

#[tokio::test]
async fn stale_unknown_and_disconnected_mcp_grants_are_rejected() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let live_identity = register_mcp_tool(&tg, "alpha", "alpha/read", "read", "Read alpha", 0x11);
    let router = build_authenticated_router(&tg);

    let live_grant = grant_json("alpha/read", &live_identity);
    let mut create = new_cron_body("Granted");
    create["mcp_tool_grants"] = json!([live_grant.clone()]);
    let created = admin_post_expect(&router, "/v1/cron", create, StatusCode::CREATED).await;
    let uri = format!("/v1/cron/{}", created["id"].as_str().expect("id"));

    let stale_identity = McpTransportIdentity::from_sha256([0x99; 32]);
    let stale = admin_patch_expect(
        &router,
        &uri,
        json!({"mcp_tool_grants": [grant_json("alpha/read", &stale_identity)]}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        stale["error"]
            .as_str()
            .unwrap_or("")
            .contains("stale MCP grant")
    );
    let unchanged = admin_get(&router, &uri, StatusCode::OK).await;
    assert_eq!(unchanged["mcp_tool_grants"], json!([live_grant.clone()]));

    tg.deps.tool_registry.unregister_for_source("alpha");
    let disconnected = admin_patch_expect(
        &router,
        &uri,
        json!({"mcp_tool_grants": [live_grant]}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        disconnected["error"]
            .as_str()
            .unwrap_or("")
            .contains("not currently live")
    );

    let mut unknown_create = new_cron_body("Unknown grant");
    unknown_create["mcp_tool_grants"] = json!([grant_json("missing/tool", &stale_identity)]);
    let unknown =
        admin_post_expect(&router, "/v1/cron", unknown_create, StatusCode::BAD_REQUEST).await;
    assert!(
        unknown["error"]
            .as_str()
            .unwrap_or("")
            .contains("not currently live")
    );
}

// ── helpers ─────────────────────────────────────────────────────────

struct TestMcpTool {
    server: String,
    name: String,
    upstream: String,
    description: String,
    transport_identity: McpTransportIdentity,
}

#[async_trait::async_trait]
impl Tool for TestMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn mcp_metadata(&self) -> Option<McpToolMetadata> {
        Some(McpToolMetadata {
            tool_name: self.name.clone(),
            server_name: self.server.clone(),
            upstream_name: self.upstream.clone(),
            transport_identity: self.transport_identity.clone(),
            transport_accesses: Vec::new(),
        })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        Ok(ToolOutput::Text(String::new()))
    }
}

fn register_mcp_tool(
    tg: &baybo_gateway::test_support::TestGateway,
    server: &str,
    name: &str,
    upstream: &str,
    description: &str,
    identity_byte: u8,
) -> McpTransportIdentity {
    register_mcp_tool_with_trust(
        tg,
        server,
        name,
        upstream,
        description,
        identity_byte,
        TrustLevel::Trusted,
    )
}

fn register_mcp_tool_with_trust(
    tg: &baybo_gateway::test_support::TestGateway,
    server: &str,
    name: &str,
    upstream: &str,
    description: &str,
    identity_byte: u8,
    trust_level: TrustLevel,
) -> McpTransportIdentity {
    let transport_identity = McpTransportIdentity::from_sha256([identity_byte; 32]);
    tg.deps.tool_registry.register_dynamic(
        server,
        Arc::new(TestMcpTool {
            server: server.to_string(),
            name: name.to_string(),
            upstream: upstream.to_string(),
            description: description.to_string(),
            transport_identity: transport_identity.clone(),
        }),
        ToolManifest {
            name: name.to_string(),
            description: description.to_string(),
            trust_level,
            parameters_schema: json!({"type": "object"}),
            capabilities: Vec::new(),
            channels: Vec::new(),
        },
    );
    transport_identity
}

fn grant_json(tool_name: &str, transport_identity: &McpTransportIdentity) -> Value {
    json!({
        "tool_name": tool_name,
        "transport_identity": transport_identity.to_string(),
    })
}

fn new_cron_body(title: &str) -> Value {
    json!({
        "schedule": "0 9 * * *",
        "user_id": "owner",
        "title": title,
        "text": "Summarize the news",
        "timezone": "UTC",
    })
}

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
    baybo_gateway::server::AdminState::from_deps(&tg.deps)
}

fn build_router(state: baybo_gateway::server::AdminState) -> axum::Router {
    let (router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    router.with_state(state)
}

fn build_authenticated_router(tg: &baybo_gateway::test_support::TestGateway) -> axum::Router {
    baybo_gateway::server::build_admin_router_for_tests(&tg.deps)
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    request_with_token(router, method, uri, body, expected, None).await
}

async fn request_with_token(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    expected: StatusCode,
    token: Option<&str>,
) -> Value {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
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

async fn admin_request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    request_with_token(router, method, uri, body, expected, Some(TEST_ADMIN_TOKEN)).await
}

async fn get(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "GET", uri, None, expected).await
}

async fn post_expect(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "POST", uri, Some(body), expected).await
}

async fn admin_get(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    admin_request(router, "GET", uri, None, expected).await
}

async fn admin_post_expect(
    router: &axum::Router,
    uri: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    admin_request(router, "POST", uri, Some(body), expected).await
}

async fn admin_patch_expect(
    router: &axum::Router,
    uri: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    admin_request(router, "PATCH", uri, Some(body), expected).await
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
