//! End-to-end exercise of the autonomous `/goal` continuation loop driven
//! through [`AgentTestHarness`] — the one architecturally invasive part of the
//! goal feature: an `Active` goal makes the actor re-fire its own turn at the
//! boundary, with no new user message, until the model marks it done.

use std::time::Duration;

use aura_channels::{AgentEvent, AgentOutput};
use aura_llm::{LlmResponse, TokenUsage, ToolCallInfo};
use aura_model::{ContentBlock, GoalStatus, TriggerSource};
use serde_json::json;

use aura_integration_tests::{AgentTestHarness, SessionBuilder};

const DRAIN: Duration = Duration::from_millis(800);

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> LlmResponse {
    LlmResponse {
        content: String::new(),
        content_blocks: vec![],
        tool_calls: vec![ToolCallInfo {
            id: id.into(),
            name: name.into(),
            arguments: args,
            signature: None,
        }],
        usage: TokenUsage::default(),
        thinking: None,
    }
}

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        content: text.into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: None,
    }
}

fn notices(outs: &[AgentOutput]) -> Vec<String> {
    outs.iter()
        .filter_map(|o| match &o.event {
            AgentEvent::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `/goal <objective>` sets the goal and the actor then drives a continuation
/// turn on its own (no new user message); when the model calls
/// `update_goal(complete)` the loop stops and a completion notice fires.
#[tokio::test]
async fn goal_command_starts_autonomous_continuation_until_complete() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    // The lone continuation turn: the model marks the goal complete, then (after
    // the tool result) emits a final progress reply.
    harness.stub_llm.push_response(tool_call(
        "c1",
        "update_goal",
        json!({ "status": "complete" }),
    ));
    harness
        .stub_llm
        .push_response(text_reply("All requirements met — done."));

    harness
        .send_text("/goal finish the migration")
        .await
        .unwrap();
    let outs = harness.drain_outputs(DRAIN).await;

    let all_notices = notices(&outs);
    assert!(
        all_notices.iter().any(|t| t.contains("Goal set")),
        "expected a goal-set notice, got {all_notices:?}"
    );

    // A continuation turn ran with NO new user message: the stub saw the framed
    // continuation steering.
    let captured = harness.stub_llm.captured_requests();
    let saw_continuation = captured.iter().any(|r| {
        r.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text(t) if t.contains("continuing autonomous work")),
            )
        })
    });
    assert!(
        saw_continuation,
        "expected an autonomous continuation turn to fire (no continuation steering seen)"
    );

    // The goal is now Complete and a completion notice was emitted.
    let goal = harness
        .goal_service
        .current(&session_id)
        .await
        .unwrap()
        .expect("goal row present");
    assert_eq!(goal.status, GoalStatus::Complete);
    assert!(
        all_notices.iter().any(|t| t.contains("Goal complete")),
        "expected a goal-complete notice, got {all_notices:?}"
    );

    harness.shutdown().await;
}

/// `/goal pause` stops the loop: the goal flips to `Paused` and no further
/// continuation turn fires.
#[tokio::test]
async fn goal_pause_stops_the_loop() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    // Keep the goal active: every continuation turn just narrates progress (no
    // update_goal), so without a pause the loop would run forever. Prime a few
    // so the first turn(s) succeed before the pause lands.
    for _ in 0..4 {
        harness
            .stub_llm
            .push_response(text_reply("still working..."));
    }

    harness.send_text("/goal keep refactoring").await.unwrap();
    // Let at least one continuation turn run.
    let _ = harness.drain_outputs(Duration::from_millis(300)).await;

    harness.send_text("/goal pause").await.unwrap();
    let outs = harness.drain_outputs(DRAIN).await;

    let goal = harness
        .goal_service
        .current(&session_id)
        .await
        .unwrap()
        .expect("goal row present");
    assert_eq!(goal.status, GoalStatus::Paused);
    assert!(
        notices(&outs).iter().any(|t| t.contains("paused")),
        "expected a paused notice, got {:?}",
        notices(&outs)
    );

    harness.shutdown().await;
}

/// A bare `/goal` with no goal set views nothing; after one is created it
/// reports the live objective + status.
#[tokio::test]
async fn goal_view_reports_status() {
    let mut harness = AgentTestHarness::builder().build();

    harness.send_text("/goal").await.unwrap();
    let outs = harness.drain_outputs(Duration::from_millis(200)).await;
    assert!(
        notices(&outs).iter().any(|t| t.contains("No goal is set")),
        "bare /goal with none set should say so: {:?}",
        notices(&outs)
    );

    // Create one, then pause immediately so the view is deterministic (no
    // continuation turns racing the assertion).
    harness.send_text("/goal climb the mountain").await.unwrap();
    harness.send_text("/goal pause").await.unwrap();
    let _ = harness.drain_outputs(Duration::from_millis(300)).await;

    harness.send_text("/goal").await.unwrap();
    let outs = harness.drain_outputs(Duration::from_millis(200)).await;
    assert!(
        notices(&outs)
            .iter()
            .any(|t| t.contains("climb the mountain")),
        "view should echo the objective: {:?}",
        notices(&outs)
    );

    harness.shutdown().await;
}

/// A non-goal-eligible session (here a cron-trigger one) must NOT be advertised
/// the goal tools — otherwise it could `create_goal` an Active row whose
/// continuation loop is never eligible to fire. Task tools (also globally
/// registered) stay visible, proving the filter is selective.
#[tokio::test]
async fn goal_tools_hidden_on_non_eligible_session() {
    let mut session = SessionBuilder::new().id("cron-sess").build();
    session.trigger = TriggerSource::Cron {
        cron_job_id: "job-1".into(),
    };
    let mut harness = AgentTestHarness::builder().session(session).build();
    harness.stub_llm.push_response(text_reply("ok"));

    harness.send_text("do something").await.unwrap();
    let _ = harness.drain_outputs(DRAIN).await;

    let captured = harness.stub_llm.captured_requests();
    let tool_names: Vec<&str> = captured
        .first()
        .expect("one LLM request")
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    for goal_tool in ["create_goal", "get_goal", "update_goal"] {
        assert!(
            !tool_names.contains(&goal_tool),
            "{goal_tool} must be hidden on a non-eligible session; saw {tool_names:?}"
        );
    }
    assert!(
        tool_names.contains(&"TaskCreate"),
        "task tools must stay visible (filter is goal-specific); saw {tool_names:?}"
    );

    harness.shutdown().await;
}
