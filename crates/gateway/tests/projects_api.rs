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

async fn seed_teammate(tg: &baybo_gateway::test_support::TestGateway, project: &str, handle: &str) {
    seed_teammate_with_id(tg, project, handle, handle).await
}

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

    get(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/issues",
        StatusCode::NOT_FOUND,
    )
    .await;
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

/// The board sets no upper bound on how much it may start at once — that is
/// the operator's call — but a negative is not a smaller number, it is a
/// number the driver would read as "empty the whole Todo column".
#[tokio::test]
async fn a_negative_run_ceiling_is_refused_rather_than_saturated() {
    let (router, _tg) = router().await;

    post(
        &router,
        "/v1/projects",
        json!({ "name": "backwards", "max_parallel_issue_runs": -1 }),
        StatusCode::BAD_REQUEST,
    )
    .await;

    let id = open_project(&router, "forwards").await;
    put(
        &router,
        &format!("/v1/projects/{id}"),
        json!({ "name": "forwards", "max_parallel_issue_runs": -1 }),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // …and a large one is simply accepted, because nothing here is a policy
    // about how many agents a board ought to have going.
    let raised = put(
        &router,
        &format!("/v1/projects/{id}"),
        json!({ "name": "forwards", "max_parallel_issue_runs": 64 }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(raised["max_parallel_issue_runs"], 64);
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

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "review", "ordered_numbers": [2] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
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

    let patched = patch(&router, &uri, json!({ "cancelled": true }), StatusCode::OK).await;
    assert!(patched["cancelled_at_ms"].is_i64());
    get(&router, &uri, StatusCode::OK).await;

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
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/move"),
        json!({ "status": "review", "ordered_numbers": [1] }),
        StatusCode::OK,
    )
    .await;

    patch(
        &router,
        &format!("/v1/projects/{p}/issues/1"),
        json!({ "assignee": "no-such-agent" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn a_board_staffs_itself_through_its_own_roster() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "staffing").await;

    let team = get(&router, &format!("/v1/projects/{p}/agents"), StatusCode::OK).await;
    let items = team["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "a new board comes with its lead");
    assert_eq!(items[0]["handle"], "lead");
    assert_eq!(items[0]["lead"], true);
    assert_eq!(items[0]["name"], "lead", "an agent's name is its handle");
    assert!(items[0].get("hired_by").is_none(), "nobody hired the lead");
    let lead_id = items[0]["id"].as_str().expect("id").to_owned();

    let hired = post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "test-engineer", "role": "Writes the tests." }),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(hired["handle"], "test-engineer");
    assert_eq!(hired["lead"], false);
    assert_eq!(hired["description"], "Writes the tests.");
    assert!(hired.get("hired_by").is_none());

    let global = get(&router, "/v1/agents", StatusCode::OK).await;
    let ids: Vec<&str> = global["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|a| a["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&lead_id.as_str()), "{ids:?}");
    assert!(!ids.contains(&hired["id"].as_str().expect("id")), "{ids:?}");

    post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "!!!", "role": "unnameable" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "roleless", "role": "  " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

/// A board addresses an agent by a `@handle` derived from its name at hire,
/// and the handle never moves — so the name must not either, through any of
/// the doors onto the `Name:` line of the agent's own `IDENTITY.md`.
#[tokio::test]
async fn a_hired_agents_name_is_its_handle_and_neither_can_be_rewritten() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "naming").await;
    let hired = post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "test-engineer", "role": "Writes the tests." }),
        StatusCode::CREATED,
    )
    .await;
    let id = hired["id"].as_str().expect("id").to_owned();
    assert_eq!(hired["handle"], "test-engineer");

    let refused = put(
        &router,
        &format!("/v1/agents/{id}/name"),
        json!({ "name": "aster" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("@handle"),
        "{refused:?}"
    );

    // The whole-file door onto the same line is shut too, version token and
    // all — otherwise the rename endpoint would just be the polite way in.
    let identity = get(
        &router,
        &format!("/v1/agents/{id}/identity"),
        StatusCode::OK,
    )
    .await;
    let body = identity["content"].as_str().expect("content");
    assert!(body.contains("* **Name:** test-engineer"), "{body}");
    put(
        &router,
        &format!("/v1/agents/{id}/identity"),
        json!({ "content": body.replace("test-engineer", "aster"), "version": identity["version"] }),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // Everything it writes around the name is still its own.
    put(
        &router,
        &format!("/v1/agents/{id}/identity"),
        json!({ "content": format!("{body}* **Vibe:** dry\n"), "version": identity["version"] }),
        StatusCode::OK,
    )
    .await;

    let team = get(&router, &format!("/v1/projects/{p}/agents"), StatusCode::OK).await;
    let member = team["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|m| m["id"] == json!(id))
        .expect("still on the team");
    // One word for one teammate: the name is the handle.
    assert_eq!(member["name"], "test-engineer");
    assert_eq!(member["handle"], "test-engineer");
}

#[tokio::test]
async fn a_removed_teammate_leaves_the_roster_but_not_the_record() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "leaving").await;
    let hired = post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "dev", "role": "Writes code." }),
        StatusCode::CREATED,
    )
    .await;
    let dev = hired["id"].as_str().expect("id").to_owned();

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
    seed_teammate(&tg, &p, "dev-1").await;
    post(
        &router,
        &format!("/v1/projects/{p}/issues"),
        json!({ "title": "do it", "status": "in_progress", "assignee": "dev-1" }),
        StatusCode::CREATED,
    )
    .await;

    let active = get(&router, &format!("/v1/projects/{p}/runs"), StatusCode::OK).await;
    let items = active["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 1);
    assert_eq!(items[0]["attempt"], 1);
    assert_eq!(items[0]["trigger"], "started");
    assert_eq!(items[0]["agent_id"], "dev-1");

    let log = get(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(log["items"].as_array().expect("items").len(), 1);

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

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/retry"),
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;

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
    let active = get(&router, &format!("/v1/projects/{p}/runs"), StatusCode::OK).await;
    assert_eq!(active["items"].as_array().expect("items").len(), 0);

    let retried = post(
        &router,
        &format!("/v1/projects/{p}/issues/1/runs/retry"),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(retried["attempt"], 2);
    assert_eq!(retried["trigger"], "retry");

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

/// One press, and every card on the board comes back at zero.
///
/// The agent comments are planted through the store because the comment
/// endpoint speaks as the operator, and the operator's own words are never
/// unread — so a fixture built out of HTTP alone would assert nothing.
#[tokio::test]
async fn one_press_reads_every_card_on_the_board() {
    let (router, tg) = router().await;
    let p = open_project(&router, "reading").await;
    seed_teammate(&tg, &p, "dev-1").await;
    let project_id = baybo_model::ProjectId::parse(p.clone()).expect("project id");

    for title in ["one", "two"] {
        let number = open_issue(&router, &p, title).await;
        let issue = tg
            .deps
            .stores
            .project
            .get_issue(&project_id, number)
            .await
            .expect("issue")
            .expect("issue row");
        tg.deps
            .stores
            .project
            .append_event(&baybo_store::project::NewIssueEvent {
                issue_id: issue.id,
                project_id: project_id.clone(),
                number,
                actor: baybo_store::project::IssueActor::Agent(
                    baybo_model::AgentProfileId::parse("dev-1").expect("agent id"),
                ),
                body: baybo_store::project::IssueEventBody::Comment {
                    text: "which way?".into(),
                    attachments: Vec::new(),
                },
            })
            .await
            .expect("agent comment");
    }

    let unread = |board: &Value| {
        board["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|card| card["unread"].as_i64().expect("unread"))
            .sum::<i64>()
    };
    let board = get(&router, &format!("/v1/projects/{p}/issues"), StatusCode::OK).await;
    assert_eq!(unread(&board), 2);

    post(
        &router,
        &format!("/v1/projects/{p}/read"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;

    let board = get(&router, &format!("/v1/projects/{p}/issues"), StatusCode::OK).await;
    assert_eq!(unread(&board), 0, "every card, not the first one");

    // A board that does not exist is a 404 rather than a quiet success on
    // nothing: the press is one the operator has to be able to trust.
    post(
        &router,
        "/v1/projects/01JGHOSTGHOSTGHOSTGHOSTGH/read",
        json!({}),
        StatusCode::NOT_FOUND,
    )
    .await;
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

#[tokio::test]
async fn a_hire_reaches_the_feed_but_the_boards_own_lead_does_not() {
    let (router, _tg) = router().await;
    let p = open_project(&router, "staffing").await;

    // Only the seeded lead so far. It arrives with the board, so it is not
    // an event and must not open every feed with "hired @lead".
    let feed = get(&router, &format!("/v1/projects/{p}/feed"), StatusCode::OK).await;
    assert_eq!(
        feed["items"].as_array().expect("items").len(),
        0,
        "a board that has done nothing has an empty feed: {feed:?}"
    );

    post(
        &router,
        &format!("/v1/projects/{p}/agents"),
        json!({ "name": "test-engineer", "role": "Writes the tests." }),
        StatusCode::CREATED,
    )
    .await;

    let feed = get(&router, &format!("/v1/projects/{p}/feed"), StatusCode::OK).await;
    let items = feed["items"].as_array().expect("items");
    assert_eq!(
        items.len(),
        1,
        "the hire is the board's only news: {items:?}"
    );
    assert_eq!(items[0]["body"]["kind"], "hired");
    assert_eq!(items[0]["body"]["agent"]["handle"], "test-engineer");
    assert!(
        items[0].get("number").is_none(),
        "a hire belongs to the board, so it points at no card: {items:?}"
    );
}

#[tokio::test]
async fn the_activity_feed_is_the_boards_timelines_read_across_it() {
    let (router, _tg) = router().await;
    let a = open_project(&router, "watched").await;
    let b = open_project(&router, "other").await;

    open_issue(&router, &a, "first").await;
    open_issue(&router, &a, "second").await;
    let posted = post(
        &router,
        &format!("/v1/projects/{a}/issues/1/comments"),
        json!({ "text": "a note on #1" }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(posted["actor"], json!({ "kind": "user" }));
    assert_eq!(posted["body"]["text"], "a note on #1");
    open_issue(&router, &b, "somebody else's").await;

    let feed = get(&router, &format!("/v1/projects/{a}/feed"), StatusCode::OK).await;
    let items = feed["items"].as_array().expect("items");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["body"]["kind"], "comment", "newest first");
    assert_eq!(items[0]["number"], 1);
    assert_eq!(items[0]["actor"], json!({ "kind": "user" }));
    assert!(
        items.iter().all(|e| e["number"] != 3),
        "another board's issue must not appear: {items:?}"
    );

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

#[tokio::test]
async fn a_timeline_names_agents_by_handle_even_after_they_leave() {
    const DEV_ID: &str = "01JC3KQ4Z8AAAAAAAAAAAAAAAA";

    let (router, tg) = router().await;
    let p = open_project(&router, "naming").await;
    seed_teammate_with_id(&tg, &p, DEV_ID, "dev-1").await;
    open_issue(&router, &p, "hand it over").await;

    patch(
        &router,
        &format!("/v1/projects/{p}/issues/1"),
        json!({ "assignee": DEV_ID }),
        StatusCode::OK,
    )
    .await;
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
                attachments: Vec::new(),
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

#[tokio::test]
async fn a_timeline_will_not_name_an_agent_from_another_board() {
    const THEIR_ID: &str = "01JC3KQ4Z8BBBBBBBBBBBBBBBB";

    let (router, tg) = router().await;
    let ours = open_project(&router, "ours").await;
    let theirs = open_project(&router, "theirs").await;
    seed_teammate(&tg, &ours, "dev-1").await;
    seed_teammate_with_id(&tg, &theirs, THEIR_ID, "dev-1").await;
    open_issue(&router, &ours, "our card").await;

    tg.deps
        .project_manager
        .record_event(
            &baybo_model::ProjectId::parse(ours.clone()).expect("project id"),
            1,
            baybo_store::project::IssueActor::Agent(
                baybo_model::AgentProfileId::parse(THEIR_ID).expect("agent id"),
            ),
            baybo_store::project::IssueEventBody::Comment {
                text: "wrong board".to_owned(),
                attachments: Vec::new(),
            },
        )
        .await;

    let events = issue_events(&router, &ours, 1).await;
    let comment = events
        .iter()
        .find(|e| e["body"]["kind"] == "comment")
        .expect("the entry is on the timeline");
    assert_eq!(comment["actor"]["kind"], "agent");
    assert_eq!(
        comment["actor"]["handle"], THEIR_ID,
        "an id this board cannot name renders as the id, not as somebody else's handle"
    );
}

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

#[tokio::test]
async fn answering_an_approval_has_to_name_a_card_on_this_board() {
    let (router, tg) = router().await;
    install_owner_channel(&tg);
    let p = open_project(&router, "approving").await;
    open_issue(&router, &p, "needs a hand").await;

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
    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/c1"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;

    open_issue(&router, &p, "a card that did not ask").await;
    let other = open_project(&router, "somebody else's").await;
    open_issue(&router, &other, "their card").await;
    let session = issue_session(&tg, &p, 1).await;
    let blocked = park_approval(&tg, &session, "c-real").await;
    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");

    let asked = issue_events(&router, &p, 1)
        .await
        .into_iter()
        .find(|e| e["body"]["kind"] == "approval_requested")
        .expect("the gate put the prompt on the card that raised it");
    assert_eq!(asked["body"]["call_id"], "c-real");
    let call_id = asked["body"]["call_id"]
        .as_str()
        .expect("call id")
        .to_owned();

    post(
        &router,
        &format!("/v1/projects/{other}/issues/1/approvals/{call_id}"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "and the prompt is still waiting"
    );
    post(
        &router,
        &format!("/v1/projects/{p}/issues/2/approvals/{call_id}"),
        json!({ "decision": "approve" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        channel.pending_approvals(&session).len(),
        1,
        "still waiting"
    );

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/{call_id}"),
        json!({ "decision": "approve" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        blocked.await.expect("the blocked call returns"),
        baybo_tools::ApprovalOutcome::answered(baybo_model::ApprovalDecision::Approve)
    );
}

fn install_owner_channel(tg: &baybo_gateway::test_support::TestGateway) {
    baybo_gateway::channel::boot::install_channels(
        &tg.deps.channel_registry,
        &tg.deps.config.channels,
    )
    .expect("install the owner channel");
    baybo_gateway::channel::boot::install_timeline_approval_gate(
        &tg.deps.channel_registry,
        std::sync::Arc::clone(&tg.deps.project_manager),
        tg.deps.session_manager.store(),
    );
}

async fn issue_session(
    tg: &baybo_gateway::test_support::TestGateway,
    project: &str,
    number: i64,
) -> baybo_model::SessionId {
    let project = baybo_model::ProjectId::parse(project.to_owned()).expect("project id");
    let issue = tg
        .deps
        .project_manager
        .get_issue(&project, number)
        .await
        .expect("the card exists");
    let channel = baybo_model::ChannelType::owner();
    tg.deps
        .session_manager
        .create_bound_session_with_trigger(
            baybo_model::User {
                id: "owner".to_owned(),
                name: None,
                channel: channel.clone(),
            },
            channel,
            baybo_model::TriggerSource::Issue {
                project_id: project,
                issue_id: issue.id,
                number,
            },
            None,
        )
        .await
        .expect("mint the issue's session")
        .id
}

async fn park_approval(
    tg: &baybo_gateway::test_support::TestGateway,
    session: &baybo_model::SessionId,
    call_id: &str,
) -> tokio::task::JoinHandle<baybo_tools::ApprovalOutcome> {
    let gate = tg
        .deps
        .channel_registry
        .approval_gates()
        .get(&baybo_model::ChannelType::owner(), session);
    park_through(tg, gate, session, call_id).await
}

async fn park_through(
    tg: &baybo_gateway::test_support::TestGateway,
    gate: std::sync::Arc<dyn baybo_tools::ApprovalGate>,
    session: &baybo_model::SessionId,
    call_id: &str,
) -> tokio::task::JoinHandle<baybo_tools::ApprovalOutcome> {
    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");
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
        if !channel.pending_approvals(session).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(channel.pending_approvals(session).len(), 1, "prompt parked");
    blocked
}

#[tokio::test]
async fn answering_a_prompt_does_not_depend_on_its_timeline_entry() {
    let (router, tg) = router().await;
    install_owner_channel(&tg);
    let p = open_project(&router, "no entry").await;
    open_issue(&router, &p, "still blocked").await;
    let session = issue_session(&tg, &p, 1).await;

    let channel = tg
        .deps
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .expect("owner channel");
    let blocked = park_through(
        &tg,
        channel.approval_gate().expect("approval gate"),
        &session,
        "c-unwritten",
    )
    .await;
    assert!(
        issue_events(&router, &p, 1)
            .await
            .iter()
            .all(|e| e["body"]["kind"] != "approval_requested"),
        "the card shows no prompt, which is the situation under test"
    );

    post(
        &router,
        &format!("/v1/projects/{p}/issues/1/approvals/c-unwritten"),
        json!({ "decision": "deny" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        blocked.await.expect("the blocked call returns"),
        baybo_tools::ApprovalOutcome::answered(baybo_model::ApprovalDecision::Deny)
    );
}

#[tokio::test]
async fn a_prompt_from_a_chat_session_cannot_be_answered_from_a_card() {
    let (router, tg) = router().await;
    install_owner_channel(&tg);
    let p = open_project(&router, "not a door").await;
    open_issue(&router, &p, "unrelated").await;

    let session = baybo_model::SessionId::new();
    let _blocked = park_approval(&tg, &session, "c-chat").await;
    assert!(
        issue_events(&router, &p, 1)
            .await
            .iter()
            .all(|e| e["body"]["kind"] != "approval_requested"),
        "a chat's prompt lands on no card"
    );

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
