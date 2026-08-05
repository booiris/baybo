//! `/v1/turns` pagination + filtering contract.
//!
//! Pins the page-size cap, the cursor round-trip, and the
//! status / session filters so the wire shape can't drift silently.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use baybo_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};
use baybo_model::{ContentBlock, SessionId, TriggerKind};
use baybo_turn::TurnInput;

fn auth(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_TOKEN}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

async fn build_router_with_seeded_turns(sessions: &[(&str, TriggerKind, usize)]) -> axum::Router {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    for (sid, trigger, count) in sessions {
        for _ in 0..*count {
            let input = match trigger {
                TriggerKind::User => TurnInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                TriggerKind::Cron => TurnInput::Cron {
                    action_payload: serde_json::json!({}),
                },
                TriggerKind::Spawned => TurnInput::Spawned {
                    initial_prompt: vec![ContentBlock::Text("task".into())],
                },
            };
            tg.deps
                .turn_lifecycle
                .start_turn(SessionId::from(*sid), *trigger, input, None)
                .await
                .expect("seed turn");
        }
    }

    use baybo_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = baybo_gateway::server::AdminState::from_deps(&tg.deps);
    let (admin_router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ))
}

async fn list_turns(router: &axum::Router, query: &str) -> (StatusCode, Value) {
    let req = auth(
        Request::builder()
            .method("GET")
            .uri(format!("/v1/turns{query}"))
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&body).expect("json body");
    (status, json)
}

#[tokio::test]
async fn list_turns_paginates_through_cursor() {
    let router = build_router_with_seeded_turns(&[("s-page", TriggerKind::User, 7)]).await;

    // First page: limit=3 → items.len() == 3, next_cursor present.
    let (st, body) = list_turns(&router, "?limit=3").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    let cursor = body["next_cursor"]
        .as_str()
        .expect("next_cursor present on partial page")
        .to_string();
    assert_eq!(cursor, "3");

    // Second page: 3 more.
    let (st, body) = list_turns(&router, &format!("?limit=3&cursor={cursor}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    let cursor2 = body["next_cursor"].as_str().unwrap().to_string();

    // Third page: 1 remaining, no cursor.
    let (st, body) = list_turns(&router, &format!("?limit=3&cursor={cursor2}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body.get("next_cursor").is_none());
}

#[tokio::test]
async fn list_turns_filters_by_session() {
    let router = build_router_with_seeded_turns(&[
        ("s-a", TriggerKind::User, 2),
        ("s-b", TriggerKind::User, 4),
    ])
    .await;

    let (st, body) = list_turns(&router, "?session=s-b").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 4);
    for item in body["items"].as_array().unwrap() {
        assert_eq!(item["session_id"], "s-b");
    }
}

#[tokio::test]
async fn list_turns_invalid_status_returns_400() {
    let router = build_router_with_seeded_turns(&[("s-x", TriggerKind::User, 1)]).await;
    let (st, _body) = list_turns(&router, "?status=submitted").await;
    // Pre-redesign wire used `submitted`/`accepted`; reject as a client
    // error so operators don't silently get an empty list.
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_turns_caps_oversized_limit() {
    let router = build_router_with_seeded_turns(&[("s-cap", TriggerKind::User, 10)]).await;
    let (st, body) = list_turns(&router, "?limit=10000").await;
    assert_eq!(st, StatusCode::OK);
    // We seeded 10; with ceiling the list still returns all of them
    // and no next_cursor.
    assert_eq!(body["items"].as_array().unwrap().len(), 10);
    assert!(body.get("next_cursor").is_none());
}
