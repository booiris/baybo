//! Integration coverage for the admin-side `/v1/agents` REST surface.
//!
//! Walks the CRUD contract from `docs/modules/agent-profiles.md`: the
//! seeded builtin is listed first and locked (400 on content update /
//! delete, avatar allowed), creates validate name/llm/avatar-blob, `PUT`
//! is a full content replace, and deletes are plain row removals.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_gateway::test_support::build_test_deps;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn agents_api_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state);

    // ── 1. Fresh install lists exactly the locked builtin ───────────
    let list = get(&router, "/v1/agents", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "fresh DB has only the builtin: {items:?}");
    let builtin = &items[0];
    assert_eq!(builtin["id"].as_str(), Some("baybo"));
    assert_eq!(builtin["name"].as_str(), Some("baybo"));
    assert_eq!(builtin["builtin"].as_bool(), Some(true));
    assert_eq!(builtin["framework"].as_str(), Some("baybo"));
    // NULL = inherit-default fields are absent on the wire.
    for inherit in ["llm", "avatar_blob_id"] {
        assert!(
            builtin.get(inherit).is_none(),
            "builtin {inherit} must be absent (inherit), got {builtin:?}",
        );
    }

    // ── 2. Create validates name / llm / avatar blob ─────────────────
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "   " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(err["error"].as_str().unwrap_or("").contains("empty"));

    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "llm": "nope" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown LLM entry")
    );

    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "avatar_blob_id": "sha256:feed.beef" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown blob id")
    );

    // A real but non-image blob is rejected too.
    let text_blob = tg
        .deps
        .stores
        .blob
        .put(b"not an image", "text/plain", None)
        .await
        .expect("upload text blob");
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "avatar_blob_id": text_blob.blob_id }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("must be an image")
    );

    // ── 3. Create round-trips (trimmed name, image avatar, lists) ────
    let png_blob = tg
        .deps
        .stores
        .blob
        .put(&[0x89, b'P', b'N', b'G'], "image/png", None)
        .await
        .expect("upload png blob");
    let llm_entry = tg
        .deps
        .llm_pool
        .read()
        .entry_names()
        .first()
        .expect("test pool has one entry")
        .to_string();
    let created = post_expect(
        &router,
        "/v1/agents",
        json!({
            "name": "  Helper  ",
            "description": "test persona",
            "framework": "claude",
            "soul": "Be terse.",
            "llm": llm_entry,
            "avatar_blob_id": png_blob.blob_id,
        }),
        StatusCode::OK,
    )
    .await;
    let agent_id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["name"].as_str(), Some("Helper"), "name is trimmed");
    assert_eq!(created["builtin"].as_bool(), Some(false));
    assert_eq!(created["framework"].as_str(), Some("claude"));
    assert_eq!(created["llm"].as_str(), Some(llm_entry.as_str()));

    let fetched = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(fetched["name"].as_str(), Some("Helper"));
    assert_eq!(fetched["created_at"], created["created_at"]);

    // Names are not unique any more, and cannot be: they live in a file the
    // agent rewrites at will, so no constraint could hold. The id is the
    // identity — every binding, partition and lookup keys off it — and a
    // duplicate name is only a display ambiguity.
    let twin = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "hELPer" }),
        StatusCode::OK,
    )
    .await;
    assert_ne!(twin["id"].as_str(), created["id"].as_str());
    delete_expect(
        &router,
        &format!("/v1/agents/{}", twin["id"].as_str().expect("id")),
        StatusCode::NO_CONTENT,
    )
    .await;

    // ── 4. Builtin: only its framework is pinned ────────────────────
    // Its description is ordinary editable text.
    put_expect(
        &router,
        "/v1/agents/baybo",
        json!({ "description": "my own words", "framework": "baybo" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let builtin = get(&router, "/v1/agents/baybo", StatusCode::OK).await;
    assert_eq!(builtin["description"].as_str(), Some("my own words"));

    // Its framework is not: baybo is what makes this row the default.
    let err = put_expect(
        &router,
        "/v1/agents/baybo",
        json!({ "description": "", "framework": "claude" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(err["error"].as_str().unwrap_or("").contains("baybo"));
    let unchanged = get(&router, "/v1/agents/baybo", StatusCode::OK).await;
    assert_eq!(
        unchanged["description"].as_str(),
        Some("my own words"),
        "a refused framework change must not have applied the rest",
    );

    let err = delete_expect(&router, "/v1/agents/baybo", StatusCode::BAD_REQUEST).await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("cannot be deleted")
    );

    // …but every field with a targeted endpoint is editable. What the lock
    // protects is the row's claim to *be* default behaviour — its framework
    // and description — not which model it runs on or what it calls itself.
    // …its model is not one of them: the builtin *is* `default-llm`, so
    // pinning it would put one decision in two places.
    let err = put_expect(
        &router,
        "/v1/agents/baybo/model",
        json!({ "llm": llm_entry }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(err["error"].as_str().unwrap_or("").contains("default-llm"));
    let builtin = get(&router, "/v1/agents/baybo", StatusCode::OK).await;
    assert!(builtin.get("llm").is_none(), "got {builtin:?}");

    put_expect(
        &router,
        "/v1/agents/baybo/name",
        json!({ "name": "Aster" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let builtin = get(&router, "/v1/agents/baybo", StatusCode::OK).await;
    assert_eq!(
        builtin["name"].as_str(),
        Some("Aster"),
        "the builtin's name is the workspace IDENTITY.md, and it is editable",
    );

    put_expect(
        &router,
        "/v1/agents/baybo/avatar",
        json!({ "blob_id": png_blob.blob_id }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let listed = get(&router, "/v1/agents", StatusCode::OK).await;
    let builtin = &listed["items"].as_array().expect("items")[0];
    assert_eq!(
        builtin["avatar_blob_id"].as_str(),
        Some(png_blob.blob_id.as_str()),
    );
    put_expect(
        &router,
        "/v1/agents/baybo/avatar",
        json!({ "blob_id": null }),
        StatusCode::NO_CONTENT,
    )
    .await;

    // ── 4b. The soul is a file, edited on its own endpoint ──────────
    // Created with the body's `soul`, so a one-call create still gives the
    // agent a persona.
    let soul = get(
        &router,
        &format!("/v1/agents/{agent_id}/soul"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(soul["content"].as_str(), Some("Be terse."));
    assert!(
        soul["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&format!("personas/{agent_id}/SOUL.md")),
        "soul must live in the agent's own persona dir, got {soul:?}",
    );

    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/soul"),
        json!({ "content": "# Helper\n\nRewritten." }),
        StatusCode::OK,
    )
    .await;
    let soul = get(
        &router,
        &format!("/v1/agents/{agent_id}/soul"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(soul["content"].as_str(), Some("# Helper\n\nRewritten."));

    // The self-image is a second per-agent file with the same treatment.
    let identity = get(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        StatusCode::OK,
    )
    .await;
    assert!(
        identity["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&format!("personas/{agent_id}/IDENTITY.md")),
        "self-image must live in the agent's own persona dir, got {identity:?}",
    );
    assert!(
        identity["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Who Am I"),
        "seeded from the shipped template, got {identity:?}",
    );
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "* **Name:** Vega\n" }),
        StatusCode::OK,
    )
    .await;
    let identity = get(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(identity["content"].as_str(), Some("* **Name:** Vega\n"));

    // ── 4c. A stale editor cannot delete what the agent wrote ───────
    // The web renders these files without polling or subscribing, so it is
    // routinely stale. The version token is what makes that safe: a Save
    // from an editor opened before a self-edit is refused, not applied.
    let stale = get(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        StatusCode::OK,
    )
    .await;
    let stale_version = stale["version"].as_str().expect("version").to_owned();
    // …the agent rewrites the file underneath (what `Edit` does mid-turn).
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "* **Name:** Chosen by the agent\n" }),
        StatusCode::OK,
    )
    .await;
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "clobber", "version": stale_version }),
        StatusCode::CONFLICT,
    )
    .await;
    let survived = get(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        survived["content"].as_str(),
        Some("* **Name:** Chosen by the agent\n"),
        "the agent's self-edit must survive a stale Save",
    );
    // Re-reading yields a version that writes cleanly.
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({
            "content": "* **Name:** Vega\n",
            "version": survived["version"].as_str().expect("version"),
        }),
        StatusCode::OK,
    )
    .await;
    // An omitted version is still an unconditional write, for callers that
    // genuinely mean "set it to this".
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "* **Name:** Vega\n" }),
        StatusCode::OK,
    )
    .await;

    // ── 4d. The name is IDENTITY.md, from both directions ───────────
    // Renaming through the API rewrites the `Name:` line and leaves every
    // other line the agent wrote alone.
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "# Who Am I?\n\n* **Name:** Vega\n* **Vibe:** dry\n" }),
        StatusCode::OK,
    )
    .await;
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/name"),
        json!({ "name": "Renamed" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let identity = get(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        StatusCode::OK,
    )
    .await;
    let body = identity["content"].as_str().unwrap_or_default();
    assert!(body.contains("* **Name:** Renamed"), "{body}");
    assert!(
        body.contains("* **Vibe:** dry"),
        "the rest survives: {body}"
    );

    // And the other direction — what the agent writes into the file is what
    // the roster shows, with no column to keep in step.
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "# Who Am I?\n\n* **Name:** Chosen by the agent\n" }),
        StatusCode::OK,
    )
    .await;
    let fetched = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(fetched["name"].as_str(), Some("Chosen by the agent"));

    // An unnamed agent falls back to its id rather than rendering blank.
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/identity"),
        json!({ "content": "# Who Am I?\n\nno name line at all\n" }),
        StatusCode::OK,
    )
    .await;
    let fetched = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(fetched["name"].as_str(), Some(agent_id.as_str()));

    // The built-in's pair is the workspace's own — editable, even though its
    // row is locked, because these are files, not row fields.
    let builtin_soul = get(&router, "/v1/agents/baybo/soul", StatusCode::OK).await;
    assert!(
        builtin_soul["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("profile/SOUL.md"),
        "the builtin's soul is the workspace one, got {builtin_soul:?}",
    );
    put_expect(
        &router,
        "/v1/agents/baybo/soul",
        json!({ "content": "workspace soul, edited" }),
        StatusCode::OK,
    )
    .await;
    let builtin_identity = get(&router, "/v1/agents/baybo/identity", StatusCode::OK).await;
    assert!(
        builtin_identity["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("profile/IDENTITY.md"),
        "got {builtin_identity:?}",
    );

    // A malformed id can never reach the filesystem.
    get(
        &router,
        "/v1/agents/..%2F..%2Fetc/soul",
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 4e. The skills readout is per-agent ─────────────────────────
    // The harness registry starts empty; the compiled-in skills are what
    // make "shared vs scoped" observable at all.
    assert!(tg.deps.skill_registry.register_builtins() > 0);
    // The shared listing belongs to the built-in; a custom agent sees only
    // its own overlay plus the universal skills, so the Agents page cannot
    // show one agent's inventory while editing another.
    let shared = get(&router, "/v1/skills", StatusCode::OK).await;
    let shared_names: Vec<&str> = shared["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|s| s["name"].as_str().unwrap_or_default())
        .collect();
    assert!(shared_names.contains(&"deck"), "{shared_names:?}");

    let scoped = get(
        &router,
        &format!("/v1/skills?agent_id={agent_id}"),
        StatusCode::OK,
    )
    .await;
    let scoped_names: Vec<&str> = scoped["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|s| s["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        !scoped_names.contains(&"deck"),
        "a custom agent must not inherit the shared set: {scoped_names:?}"
    );
    assert!(scoped_names.contains(&"baybo-cli"), "{scoped_names:?}");
    assert!(
        scoped["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|s| s["universal"].as_bool() == Some(true)),
        "a fresh agent has only universal skills: {scoped_names:?}",
    );

    // A malformed id is a 400 here too, not a silently global listing.
    get(
        &router,
        "/v1/skills?agent_id=../escape",
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 5. PUT is a full replace of what the row still owns ─────────
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        json!({ "description": "", "framework": "baybo" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let replaced = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(replaced["framework"].as_str(), Some("baybo"));
    // Three things the content PUT does NOT touch, each because it has its
    // own targeted endpoint: the avatar, the LLM pin, and the name (which is
    // not a row field at all).
    assert_eq!(
        replaced["avatar_blob_id"].as_str(),
        Some(png_blob.blob_id.as_str()),
    );
    assert_eq!(replaced["llm"].as_str(), Some(llm_entry.as_str()));
    assert_eq!(
        replaced["name"].as_str(),
        Some(agent_id.as_str()),
        "the file was left without a Name: line above, so it falls back to the id",
    );

    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}/name"),
        json!({ "name": "Helper 2" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let named = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(named["name"].as_str(), Some("Helper 2"));

    let beta = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Beta" }),
        StatusCode::OK,
    )
    .await;
    let beta_id = beta["id"].as_str().expect("id").to_owned();
    // A rename onto another agent's name is allowed — see above.
    put_expect(
        &router,
        &format!("/v1/agents/{beta_id}/name"),
        json!({ "name": "helper 2" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    delete_expect(
        &router,
        &format!("/v1/agents/{beta_id}"),
        StatusCode::NO_CONTENT,
    )
    .await;

    // Unknown ids 404 across the surface.
    get(&router, "/v1/agents/missing", StatusCode::NOT_FOUND).await;
    put_expect(
        &router,
        "/v1/agents/missing",
        json!({ "description": "", "framework": "baybo" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    delete_expect(&router, "/v1/agents/missing", StatusCode::NOT_FOUND).await;

    // ── 6. Delete removes the custom row ─────────────────────────────
    delete_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        StatusCode::NO_CONTENT,
    )
    .await;
    get(
        &router,
        &format!("/v1/agents/{agent_id}"),
        StatusCode::NOT_FOUND,
    )
    .await;
    let list = get(&router, "/v1/agents", StatusCode::OK).await;
    assert_eq!(list["items"].as_array().expect("items").len(), 1);
}

// ── helpers ─────────────────────────────────────────────────────────

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
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
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

async fn put_expect(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "PUT", uri, Some(body), expected).await
}

async fn delete_expect(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "DELETE", uri, None, expected).await
}
