//! `/v1/projects/*` — the kanban board's HTTP surface.
//!
//! The through-line these tests protect: an issue is addressed as
//! `(project, number)`, so no request can reach a card on another board,
//! and a move is one transaction that leaves the destination column
//! densely ordered.

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn router() -> (axum::Router, baybo_gateway::test_support::TestGateway) {
    let tg =
        baybo_gateway::test_support::build_test_deps("127.0.0.1:0".parse().expect("addr")).await;
    let state = baybo_gateway::server::AdminState::from_deps(&tg.deps);
    let (router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    (router.with_state(state), tg)
}

/// Open a project and return its id. Workdir is left to the server, which
/// materialises one under the per-test workspace.
async fn open_project(router: &axum::Router, name: &str) -> String {
    let created = post(
        router,
        "/v1/projects",
        json!({ "name": name }),
        StatusCode::CREATED,
    )
    .await;
    created["id"].as_str().expect("id").to_owned()
}

/// Put a runnable agent on a board's team. Assignment is roster-scoped, so
/// the profile has to name the project it is being assigned inside.
async fn seed_teammate(tg: &baybo_gateway::test_support::TestGateway, project: &str, handle: &str) {
    let now = chrono::Utc::now();
    tg.deps
        .stores
        .agent_profile
        .create(&baybo_store::AgentProfileRow {
            id: baybo_model::AgentProfileId::parse(handle).expect("agent id"),
            description: String::new(),
            avatar_blob_id: None,
            framework: baybo_model::AgentFramework::Baybo,
            llm: None,
            builtin: false,
            team: Some(baybo_model::TeamMembership {
                project_id: baybo_model::ProjectId::parse(project.to_owned()).expect("project id"),
                handle: baybo_model::AgentHandle::parse(handle).expect("handle"),
            }),
            hired_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("seed agent");
}

async fn open_issue(router: &axum::Router, project: &str, title: &str) -> i64 {
    let created = post(
        router,
        &format!("/v1/projects/{project}/issues"),
        json!({ "title": title }),
        StatusCode::CREATED,
    )
    .await;
    created["number"].as_i64().expect("number")
}

#[tokio::test]
async fn a_project_opens_with_a_repo_and_an_empty_board() {
    let (router, _tg) = router().await;

    let created = post(
        &router,
        "/v1/projects",
        json!({ "name": "Kanban", "description": "the board" }),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["name"], "Kanban");
    let workdir = created["workdir"].as_str().expect("workdir");
    assert!(
        std::path::Path::new(workdir).join(".git").exists(),
        "an omitted workdir is materialised as a repository"
    );
    assert!(
        created.get("archived_at_ms").is_none(),
        "a fresh project is not in the archive"
    );

    let id = created["id"].as_str().expect("id");
    let board = get(
        &router,
        &format!("/v1/projects/{id}/issues"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(board["items"].as_array().expect("items").len(), 0);

    // An unknown board is a 404, not an empty board.
    get(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/issues",
        StatusCode::NOT_FOUND,
    )
    .await;
    // A path segment that fails the id grammar never reaches the store.
    get(&router, "/v1/projects/..%2Fetc", StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn a_blank_name_and_a_bad_workdir_are_refused() {
    let (router, _tg) = router().await;

    post(
        &router,
        "/v1/projects",
        json!({ "name": "  " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    post(
        &router,
        "/v1/projects",
        json!({ "name": "relative", "workdir": "not/absolute" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn issues_are_numbered_per_project_and_never_cross_boards() {
    let (router, _tg) = router().await;
    let a = open_project(&router, "alpha").await;
    let b = open_project(&router, "beta").await;

    assert_eq!(open_issue(&router, &a, "a's first").await, 1);
    assert_eq!(open_issue(&router, &a, "a's second").await, 2);
    assert_eq!(
        open_issue(&router, &b, "b's first").await,
        1,
        "numbering restarts inside each project"
    );

    // Both boards have a #1 and each request only ever reaches its own.
    let from_a = get(
        &router,
        &format!("/v1/projects/{a}/issues/1"),
        StatusCode::OK,
    )
    .await;
    let from_b = get(
        &router,
        &format!("/v1/projects/{b}/issues/1"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(from_a["title"], "a's first");
    assert_eq!(from_b["title"], "b's first");

    // b has no #2, even though a does.
    get(
        &router,
        &format!("/v1/projects/{b}/issues/2"),
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn a_move_relocates_the_card_and_renumbers_its_column() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "moves").await;
    for title in ["one", "two", "three"] {
        open_issue(&router, &p, title).await;
    }

    // #3 crosses into Todo; Backlog closes ranks behind it.
    let moved = post(
        &router,
        &format!("/v1/projects/{p}/issues/3/move"),
        json!({ "status": "todo", "ordered_numbers": [3] }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(moved["status"], "todo");
    assert_eq!(moved["position"], 0);

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "backlog", "ordered_numbers": [2, 1] }),
        StatusCode::OK,
    )
    .await;
    let board = get(&router, &format!("/v1/projects/{p}/issues"), StatusCode::OK).await;
    let items = board["items"].as_array().expect("items");
    let find = |number: i64| {
        items
            .iter()
            .find(|i| i["number"] == number)
            .expect("issue present")
    };
    assert_eq!(find(2)["position"], 0);
    assert_eq!(find(1)["position"], 1);

    // A destination list that omits the moved card would leave it unplaced.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "review", "ordered_numbers": [2] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    // An unknown card cannot be moved.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/99/move"),
        json!({ "status": "done", "ordered_numbers": [99] }),
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn a_patch_leaves_unnamed_fields_alone_and_null_clears_the_block() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "patches").await;
    open_issue(&router, &p, "original").await;
    let uri = format!("/v1/projects/{p}/issues/1");

    let patched = patch(
        &router,
        &uri,
        json!({ "description": "filled in", "blocked_reason": "waiting on tmux" }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        patched["title"], "original",
        "an unnamed title is untouched"
    );
    assert_eq!(patched["description"], "filled in");
    assert_eq!(patched["blocked_reason"], "waiting on tmux");

    // An absent key leaves the block; an explicit null clears it. Plain
    // `Option<Option<_>>` cannot tell those apart, which is why the field
    // has a custom deserializer.
    let patched = patch(&router, &uri, json!({ "priority": "high" }), StatusCode::OK).await;
    assert_eq!(patched["blocked_reason"], "waiting on tmux");
    assert_eq!(patched["priority"], "high");

    let patched = patch(
        &router,
        &uri,
        json!({ "blocked_reason": null }),
        StatusCode::OK,
    )
    .await;
    assert!(patched.get("blocked_reason").is_none(), "null unblocked it");

    // Cancel keeps the row; it just stops being live work.
    let patched = patch(&router, &uri, json!({ "cancelled": true }), StatusCode::OK).await;
    assert!(patched["cancelled_at_ms"].is_i64());
    get(&router, &uri, StatusCode::OK).await;

    // A body that sets nothing is a caller mistake, not a silent no-op.
    patch(&router, &uri, json!({}), StatusCode::BAD_REQUEST).await;
    patch(
        &router,
        &uri,
        json!({ "title": "   " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn an_archived_project_leaves_the_listing_and_stops_taking_writes() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "archivable").await;
    open_issue(&router, &p, "before").await;

    let archived = post(
        &router,
        &format!("/v1/projects/{p}/archive"),
        json!({ "archived": true }),
        StatusCode::OK,
    )
    .await;
    assert!(archived["archived_at_ms"].is_i64());

    let listed = get(&router, "/v1/projects", StatusCode::OK).await;
    assert_eq!(listed["items"].as_array().expect("items").len(), 0);
    let listed = get(
        &router,
        "/v1/projects?include_archived=true",
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);

    // Reading works; writing does not. That is the whole difference
    // between archiving and deleting.
    get(&router, &format!("/v1/projects/{p}/issues"), StatusCode::OK).await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "after" }),
        StatusCode::CONFLICT,
    )
    .await;
    put(
        &router,
        &format!("/v1/projects/{p}"),
        json!({ "name": "renamed" }),
        StatusCode::CONFLICT,
    )
    .await;

    // And it comes back.
    post(
        &router,
        &format!("/v1/projects/{p}/archive"),
        json!({ "archived": false }),
        StatusCode::OK,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "after restore" }),
        StatusCode::CREATED,
    )
    .await;
}

#[tokio::test]
async fn a_card_can_open_straight_into_a_column() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "columns").await;

    let created = post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "starts in review", "status": "review", "priority": "urgent" }),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["status"], "review");
    assert_eq!(created["priority"], "urgent");
    assert_eq!(created["position"], 0);

    // The default is the backlog, and each column ranks from zero.
    let created = post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "starts in backlog" }),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["status"], "backlog");
    assert_eq!(created["position"], 0);
}

#[tokio::test]
async fn in_progress_is_refused_without_an_assignee() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "staffing").await;
    open_issue(&router, &p, "unclaimed").await;

    // The board must not claim work is under way that nobody is on.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "in_progress", "ordered_numbers": [1] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "starts unclaimed", "status": "in_progress" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    // Every other column takes unassigned work.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "review", "ordered_numbers": [1] }),
        StatusCode::OK,
    )
    .await;

    // An agent that does not exist is refused before it can be stored.
    patch(
        &router,
        &format!("/v1/projects/{p}/issues/1"),
        json!({ "assignee": "no-such-agent" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

/// The team surface is the board's roster: it opens with a lead, hires get
/// handles, and nothing on it leaks into `/v1/agents`.
#[tokio::test]
async fn a_board_staffs_itself_through_its_own_roster() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "staffing").await;

    let team = get(&router, &format!("/v1/projects/{p}/agents"), StatusCode::OK).await;
    let items = team["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "a new board comes with its lead");
    assert_eq!(items[0]["handle"], "lead");
    assert_eq!(items[0]["lead"], true);
    assert_eq!(items[0]["name"], "Lead");
    assert!(items[0].get("hired_by").is_none(), "nobody hired the lead");
    let lead_id = items[0]["id"].as_str().expect("id").to_owned();

    let hired = post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "Test Engineer", "role": "Writes the tests." }),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(hired["handle"], "test-engineer");
    assert_eq!(hired["lead"], false);
    assert_eq!(hired["description"], "Writes the tests.");
    // The operator did this one, so nobody is credited with the hire.
    assert!(hired.get("hired_by").is_none());

    // …and the global roster is untouched: a teammate is not a chat persona.
    let global = get(&router, "/v1/agents", StatusCode::OK).await;
    let ids: Vec<&str> = global["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|a| a["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&lead_id.as_str()), "{ids:?}");
    assert!(!ids.contains(&hired["id"].as_str().expect("id")), "{ids:?}");

    // A name with no handle in it is refused rather than given a ULID.
    post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "!!!", "role": "unnameable" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    // And so is a hire with no role — it is what seeds the agent's soul.
    post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "Roleless", "role": "  " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

/// Removing an agent keeps its row, because the board still has to say who
/// did the work it did.
#[tokio::test]
async fn a_removed_teammate_leaves_the_roster_but_not_the_record() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "leaving").await;
    let hired = post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "Dev", "role": "Writes code." }),
        StatusCode::CREATED,
    )
    .await;
    let dev = hired["id"].as_str().expect("id").to_owned();

    // The card it worked on keeps naming it after it leaves.
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "its work", "assignee": dev }),
        StatusCode::CREATED,
    )
    .await;
    delete(
        &router,
        &format!("/v1/projects/{p}/agents/{dev}"),
        StatusCode::NO_CONTENT,
    )
    .await;

    let team = get(&router, &format!("/v1/projects/{p}/agents"), StatusCode::OK).await;
    assert_eq!(team["items"].as_array().expect("items").len(), 1);
    let issue = get(
        &router,
        &format!("/v1/projects/{p}/issues/1"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(issue["assignee"], dev, "the card still says who was on it");

    // Twice is a refusal, and the lead never leaves at all.
    delete(
        &router,
        &format!("/v1/projects/{p}/agents/{dev}"),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let lead = team["items"][0]["id"].as_str().expect("id").to_owned();
    delete(
        &router,
        &format!("/v1/projects/{p}/agents/{lead}"),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn a_started_card_shows_a_run_on_the_board_and_in_its_log() {
    let (router, tg) = router().await;
    let p = open_project(&router, "runs").await;
    // An agent on this board that can actually host a run.
    seed_teammate(&tg, &p, "dev-1").await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "do it", "status": "in_progress", "assignee": "dev-1" }),
        StatusCode::CREATED,
    )
    .await;

    // The card is working, and the board learns that in one read rather
    // than a lookup per card.
    let active = get(&router, &format!("/v1/projects/{p}/runs"), StatusCode::OK).await;
    let items = active["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 1);
    assert_eq!(items[0]["attempt"], 1);
    assert_eq!(items[0]["trigger"], "started");
    assert_eq!(items[0]["agent_id"], "dev-1");

    // …and the issue's own log has it too.
    let log = get(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(log["items"].as_array().expect("items").len(), 1);

    // A card nobody started has an empty log rather than a 404.
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "not started" }),
        StatusCode::CREATED,
    )
    .await;
    let log = get(
        &router,
        &format!("/v1/projects/{p}/issues/2/runs"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(log["items"].as_array().expect("items").len(), 0);
}

#[tokio::test]
async fn a_run_can_be_stopped_and_started_again() {
    let (router, tg) = router().await;
    let p = open_project(&router, "control").await;
    seed_teammate(&tg, &p, "dev-1").await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "runaway", "status": "in_progress", "assignee": "dev-1" }),
        StatusCode::CREATED,
    )
    .await;

    // While a run is in flight, a retry is refused rather than putting a
    // second agent on the same card.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/retry"),
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;

    // Nothing has claimed the run in this harness (no router), so it is
    // still queued — cancelling settles it outright.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/cancel"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let log = get(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(log["items"][0]["status"], "cancelled");
    // …and the board stops calling the card busy.
    let active = get(&router, &format!("/v1/projects/{p}/runs"), StatusCode::OK).await;
    assert_eq!(active["items"].as_array().expect("items").len(), 0);

    // With the slot free, it can run again — as a retry, so the log says
    // where the second attempt came from.
    let retried = post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/retry"),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(retried["attempt"], 2);
    assert_eq!(retried["trigger"], "retry");

    // Cancelling when nothing is running is a caller mistake, not a no-op.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/cancel"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/cancel"),
        json!({}),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

// ── helpers ─────────────────────────────────────────────────────────────

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

async fn post(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "POST", uri, Some(body), expected).await
}

async fn put(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "PUT", uri, Some(body), expected).await
}

async fn patch(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "PATCH", uri, Some(body), expected).await
}

async fn delete(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "DELETE", uri, None, expected).await
}
