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
///
/// The id is the handle, which production never mints — see
/// [`seed_teammate_with_id`] for the shape that tells the two apart.
async fn seed_teammate(tg: &baybo_gateway::test_support::TestGateway, project: &str, handle: &str) {
    seed_teammate_with_id(tg, project, handle, handle).await
}

/// The same, with the id and the handle told apart.
///
/// `AgentProfileId::generate` is a ULID and a handle is a short name a
/// person types, so anything a reader sees that came from the id is a bug —
/// which a fixture reusing the handle as the id cannot catch.
async fn seed_teammate_with_id(
    tg: &baybo_gateway::test_support::TestGateway,
    project: &str,
    id: &str,
    handle: &str,
) {
    let now = chrono::Utc::now();
    tg.deps
        .stores
        .agent_profile
        .create(&baybo_store::AgentProfileRow {
            id: baybo_model::AgentProfileId::parse(id).expect("agent id"),
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

/// One card's timeline, as the client reads it.
async fn issue_events(router: &axum::Router, project: &str, number: i64) -> Vec<Value> {
    let body = get(
        router,
        &format!("/v1/projects/{project}/issues/{number}/events"),
        StatusCode::OK,
    )
    .await;
    body["items"].as_array().expect("items").clone()
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

/// The activity feed is derived, not stored twice: the same rows the
/// per-issue timelines render, read across the board and newest first.
#[tokio::test]
async fn the_activity_feed_is_the_boards_timelines_read_across_it() {
    let (router, _tg) = router().await;
    let a = open_project(&router, "watched").await;
    let b = open_project(&router, "other").await;

    open_issue(&router, &a, "first").await;
    open_issue(&router, &a, "second").await;
    post(
        &router,
        &format!("/v1/projects/{a}/issues/1/comments"),
        json!({ "text": "a note on #1" }),
        StatusCode::OK,
    )
    .await;
    open_issue(&router, &b, "somebody else's").await;

    let feed = get(&router, &format!("/v1/projects/{a}/feed"), StatusCode::OK).await;
    let items = feed["items"].as_array().expect("items");
    // Two opens plus one comment. The other board's card is not here.
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["body"]["kind"], "comment", "newest first");
    assert_eq!(items[0]["number"], 1);
    // The operator is a tagged actor like any other, not an absent field or
    // a bare string the client has to interpret.
    assert_eq!(items[0]["actor"], json!({ "kind": "user" }));
    assert!(
        items.iter().all(|e| e["number"] != 3),
        "another board's issue must not appear: {items:?}"
    );

    // Paging backwards from the newest entry excludes it.
    let cursor = items[0]["created_at_ms"].as_i64().expect("ms");
    let older = get(
        &router,
        &format!("/v1/projects/{a}/feed?before_ms={cursor}"),
        StatusCode::OK,
    )
    .await;
    assert!(
        older["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|e| e["created_at_ms"].as_i64().expect("ms") < cursor)
    );

    get(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/feed",
        StatusCode::NOT_FOUND,
    )
    .await;
}

/// A timeline names its agents by `@handle`, never by the ULID on the row —
/// and keeps naming them after they have left the team, which is the half a
/// roster held by the client could not do.
#[tokio::test]
async fn a_timeline_names_agents_by_handle_even_after_they_leave() {
    // A real production id: `AgentProfileId::generate` mints a ULID, and the
    // handle is a separate, shorter name.
    const DEV_ID: &str = "01JC3KQ4Z8AAAAAAAAAAAAAAAA";

    let (router, tg) = router().await;
    let p = open_project(&router, "naming").await;
    seed_teammate_with_id(&tg, &p, DEV_ID, "dev-1").await;
    open_issue(&router, &p, "hand it over").await;

    // One entry that *names* the agent…
    patch(
        &router,
        &format!("/v1/projects/{p}/issues/1"),
        json!({ "assignee": DEV_ID }),
        StatusCode::OK,
    )
    .await;
    // …and one the agent itself wrote, so the actor is an agent too.
    let project = baybo_model::ProjectId::parse(p.clone()).expect("project id");
    let dev = baybo_model::AgentProfileId::parse(DEV_ID).expect("agent id");
    tg.deps
        .project_manager
        .record_event(
            &project,
            1,
            baybo_store::project::IssueActor::Agent(dev.clone()),
            baybo_store::project::IssueEventBody::Comment {
                text: "on it".to_owned(),
            },
        )
        .await;

    let named = |events: &[Value]| -> (Value, Value) {
        let assigned = events
            .iter()
            .find(|e| e["body"]["kind"] == "assigned")
            .expect("the assignment is on the timeline")
            .clone();
        let comment = events
            .iter()
            .find(|e| e["body"]["kind"] == "comment")
            .expect("the agent's comment is on the timeline")
            .clone();
        (assigned, comment)
    };

    let (assigned, comment) = named(&issue_events(&router, &p, 1).await);
    assert_eq!(assigned["body"]["to"]["handle"], "dev-1");
    assert_eq!(
        assigned["body"]["to"]["id"], DEV_ID,
        "the id rides along, because it is what opens the profile"
    );
    assert_eq!(comment["actor"]["kind"], "agent");
    assert_eq!(comment["actor"]["handle"], "dev-1");
    assert_eq!(comment["actor"]["id"], DEV_ID);

    // The agent leaves. Its past work still names it — the roster the client
    // holds no longer has it at all, which is why the handle is resolved
    // here and not there.
    tg.deps
        .project_manager
        .remove_from_team(&project, &dev)
        .await
        .expect("the agent leaves the team");
    let team = get(&router, &format!("/v1/projects/{p}/agents"), StatusCode::OK).await;
    assert!(
        team["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|m| m["id"] != DEV_ID),
        "the roster has dropped it: {team:?}"
    );

    let (assigned, comment) = named(&issue_events(&router, &p, 1).await);
    assert_eq!(
        assigned["body"]["to"]["handle"], "dev-1",
        "a timeline is history, so it outlives the roster"
    );
    assert_eq!(comment["actor"]["handle"], "dev-1");
}

/// The budget gate acts with nobody asking, so the wire has to be able to
/// say so: "you held the run — $5.00 of the $5.00 daily budget is spent"
/// accuses the reader of a decision the board made on its own.
#[tokio::test]
async fn the_boards_own_actions_are_neither_the_operators_nor_an_agents() {
    let (router, tg) = router().await;
    let p = open_project(&router, "broke").await;
    open_issue(&router, &p, "over budget").await;

    tg.deps
        .project_manager
        .record_event(
            &baybo_model::ProjectId::parse(p.clone()).expect("project id"),
            1,
            baybo_store::project::IssueActor::System,
            baybo_store::project::IssueEventBody::BudgetExhausted {
                spent_micros: 5_000_000,
                limit_micros: 5_000_000,
            },
        )
        .await;

    let events = issue_events(&router, &p, 1).await;
    let held = events
        .iter()
        .find(|e| e["body"]["kind"] == "budget_exhausted")
        .expect("the hold is on the timeline");
    assert_eq!(held["actor"], json!({ "kind": "system" }));
}

/// The approval queue is keyed by call id alone, so the endpoint's job is
/// to refuse a request that names a card it has no business answering for.
#[tokio::test]
async fn answering_an_approval_has_to_name_a_card_on_this_board() {
    let (router, tg) = router().await;
    // The harness installs no channels, so the route would otherwise 404 at
    // the channel lookup — which looks exactly like the card check this
    // test exists to exercise. Installed first, so every 404 below is the
    // refusal it claims to be.
    baybo_gateway::channel::boot::install_channels(
        &tg.deps.channel_registry,
        &tg.deps.config.channels,
    )
    .expect("install the owner channel");
    let p = open_project(&router, "approving").await;
    open_issue(&router, &p, "needs a hand").await;

    // An unknown board and an unknown card are both 404s, and neither
    // reaches the queue.
    post(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/issues/1/approvals/c1",
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues/99/approvals/c1"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    // A real card with no prompt waiting on that call is also a 404 —
    // "answered" and "there was nothing to answer" must not look alike.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/c1"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;

    // Now a real prompt, parked on the owner channel's queue the way a
    // blocked tool call parks one, and recorded on #1's timeline the way the
    // gate records it. The queue is keyed by call id alone, so this is the
    // case that says whether the card check does anything.
    open_issue(&router, &p, "a card that did not ask").await;
    let other = open_project(&router, "somebody else's").await;
    open_issue(&router, &other, "their card").await;
    let (session, blocked) = park_approval(&tg, "c-real").await;
    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");
    // The harness installs no `TimelineApprovalGate`, so the entry the gate
    // would have written goes on by hand — it is what says which card asked.
    tg.deps
        .project_manager
        .record_event(
            &baybo_model::ProjectId::parse(p.clone()).expect("project id"),
            1,
            baybo_store::project::IssueActor::User,
            baybo_store::project::IssueEventBody::ApprovalRequested {
                call_id: "c-real".to_owned(),
                tool: "Bash".to_owned(),
                summary: "rm -rf build".to_owned(),
            },
        )
        .await;

    // A card that exists on another board must not be able to answer it…
    post(
        &router,
        &format!("/v1/projects/{other}/issues/1/approvals/c-real"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "and the prompt is still waiting"
    );
    // …nor a real card on the right board that never asked. Existing is not
    // the same as having raised it.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/2/approvals/c-real"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "still waiting"
    );

    // …while the card that raised it answers it, and the call unblocks.
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/c-real"),
        json!({ "decision": "approve" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        blocked.await.expect("the blocked call returns"),
        baybo_model::ApprovalDecision::Approve
    );
}

/// Park a real approval prompt on the owner channel's queue, the way a
/// blocked tool call parks one. Returns the session it belongs to and the
/// call still waiting on the answer.
async fn park_approval(
    tg: &baybo_gateway::test_support::TestGateway,
    call_id: &str,
) -> (
    baybo_model::SessionId,
    tokio::task::JoinHandle<baybo_model::ApprovalDecision>,
) {
    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");
    let gate = channel.approval_gate().expect("approval gate");
    let session = baybo_model::SessionId::new();
    let blocked = tokio::spawn({
        let session = session.clone();
        let call_id = call_id.to_owned();
        async move {
            gate.request(baybo_tools::ApprovalRequest {
                call_id,
                tool_call_id: None,
                session_id: session,
                user_id: "owner".to_owned(),
                tool: "Bash".to_owned(),
                accesses: Vec::new(),
                params_preview: "{}".to_owned(),
                description: None,
            })
            .await
        }
    });
    for _ in 0..200 {
        if !channel.pending_approvals(&session).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "prompt parked"
    );
    (session, blocked)
}

/// A prompt raised by an ordinary chat session belongs to no card, so no
/// card can answer it. The board API is not a second door onto the queue.
#[tokio::test]
async fn a_prompt_from_a_chat_session_cannot_be_answered_from_a_card() {
    let (router, tg) = router().await;
    baybo_gateway::channel::boot::install_channels(
        &tg.deps.channel_registry,
        &tg.deps.config.channels,
    )
    .expect("install the owner channel");
    let p = open_project(&router, "not a door").await;
    open_issue(&router, &p, "unrelated").await;

    // No timeline entry: the gate passes a non-issue session straight
    // through without writing to any card.
    let (session, _blocked) = park_approval(&tg, "c-chat").await;

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/c-chat"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "the chat's prompt is still the chat's"
    );
}

/// The lead's planning conversations: a board-scoped session that runs as
/// the lead and stays off the chat surface.
#[tokio::test]
async fn a_board_opens_conversations_with_its_lead() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "planning").await;

    let empty = get(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty["items"].as_array().expect("items").len(), 0);

    let opened = post(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    let sid = opened["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    // The prefix is how a reader tells a board's thread from a chat at a
    // glance in logs and the trace viewer.
    assert!(sid.starts_with("board-"), "{sid}");

    let listed = get(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);
    assert_eq!(listed["items"][0]["session_id"], sid);

    // A second conversation is a second thread, not a replacement — the
    // panel keeps history.
    post(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    let listed = get(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["items"].as_array().expect("items").len(), 2);

    // …and none of them is in the chat list. A board's conversations live
    // on the board.
    let chats = get(&router, "/v1/chat/sessions", StatusCode::OK).await;
    let ids: Vec<&str> = chats["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|s| s["session_id"].as_str())
        .collect();
    assert!(!ids.contains(&sid.as_str()), "{ids:?}");

    // Another board's conversations are its own.
    let other = open_project(&router, "elsewhere").await;
    let theirs = get(
        &router,
        &format!("/v1/projects/{other}/lead/conversations"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(theirs["items"].as_array().expect("items").len(), 0);

    get(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/lead/conversations",
        StatusCode::NOT_FOUND,
    )
    .await;
}

/// An archived board is read-only, so it does not open new conversations.
#[tokio::test]
async fn an_archived_board_opens_no_new_conversation() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "shelved").await;
    post(
        &router,
        &format!("/v1/projects/{p}/archive"),
        json!({ "archived": true }),
        StatusCode::OK,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/lead/conversations"),
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;
}
