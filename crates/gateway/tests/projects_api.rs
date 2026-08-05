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
