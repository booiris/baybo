//! End-to-end exercise of the autonomous `/goal` continuation loop driven
//! through [`AgentTestHarness`] — the one architecturally invasive part of the
//! goal feature: an `Active` goal makes the actor re-fire its own turn at the
//! boundary, with no new user message, until the model marks it done.

use std::time::Duration;

use aura_channels::{AgentEvent, AgentOutput};
use aura_llm::{LlmError, StreamEvent, ToolCallInfo};
use aura_model::{ContentBlock, GoalStatus, MessageSource, TriggerSource};
use serde_json::json;

use aura_integration_tests::{AgentTestHarness, SessionBuilder};

const DRAIN: Duration = Duration::from_millis(800);

// A goal continuation runs in the user's visible chat, so the actor drives it
// through the streaming LLM path — tests prime `push_stream`, not `push_response`.
fn stream_tool(id: &str, name: &str, args: serde_json::Value) -> Vec<StreamEvent> {
    vec![StreamEvent::ToolCall(ToolCallInfo {
        id: id.into(),
        name: name.into(),
        arguments: args,
        signature: None,
    })]
}

fn stream_text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::Text(text.into())]
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
    harness.stub_llm.push_stream(stream_tool(
        "c1",
        "update_goal",
        json!({ "status": "complete" }),
    ));
    harness
        .stub_llm
        .push_stream(stream_text("All requirements met — done."));

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

    // The continuation streams its live execution (tool lifecycle + answer
    // deltas), not just the final message — so the chat shows the work as it
    // happens, not only after a reload.
    let streamed_execution = outs.iter().any(|o| {
        matches!(
            o.event,
            AgentEvent::ToolStarted { .. }
                | AgentEvent::ToolCompleted { .. }
                | AgentEvent::AnswerDelta(_)
                | AgentEvent::Reasoning(_)
        )
    });
    assert!(
        streamed_execution,
        "the goal continuation turn must stream its execution live, not only persist it"
    );

    harness.shutdown().await;
}

/// `/goal <objective>` records the command as a real `User` transcript row (in
/// the canonical `/goal <objective>` form) — not just in-memory continuation
/// steering — so the thread bubble keeps the `/goal` prefix, the session reloads
/// with a real title instead of "New conversation", and the objective is part of
/// the durable agent-loop context. (The sidebar strips the prefix for its preview.)
#[tokio::test]
async fn goal_set_persists_objective_as_user_message() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    // Keep the goal active (continuation just narrates), then pause so nothing
    // races the transcript read.
    for _ in 0..4 {
        harness.stub_llm.push_stream(stream_text("working on it"));
    }
    harness
        .send_text("/goal ship the parser rewrite")
        .await
        .unwrap();
    let _ = harness.drain_outputs(Duration::from_millis(300)).await;
    harness.send_text("/goal pause").await.unwrap();
    let _ = harness.drain_outputs(DRAIN).await;

    let transcript = harness
        .session_manager
        .full_transcript(&session_id)
        .await
        .expect("transcript loads");
    let has_user_objective = transcript.iter().any(|m| {
        matches!(m.source(), MessageSource::User)
            && m.content.iter().any(
                |b| matches!(b, ContentBlock::Text(t) if t.contains("/goal ship the parser rewrite")),
            )
    });
    assert!(
        has_user_objective,
        "the objective must be a persisted `/goal …` User row, got sources {:?}",
        transcript.iter().map(|m| m.source()).collect::<Vec<_>>()
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
            .push_stream(stream_text("still working..."));
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

/// An `Active` goal already past its per-goal token budget gets exactly ONE
/// wind-down turn (carrying the BUDGET_LIMIT steering), then stops as
/// `BudgetLimited` with a notice — it does not keep firing.
#[tokio::test]
async fn goal_over_budget_winds_down_to_budget_limited() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    // Seed an Active goal that is already over budget in a single write — a zero
    // budget is over the moment the goal exists — so the actor can't observe a
    // half-seeded (create-then-accrue) state at its boundary check.
    harness
        .goal_service
        .create(&session_id, "index the archive", Some(0))
        .await
        .unwrap();
    // The lone wind-down turn: the model narrates and does NOT call update_goal.
    harness
        .stub_llm
        .push_stream(stream_text("Wrapped up; here is where things stand."));

    // Bare `/goal` runs no LLM turn but wakes the actor to re-check the boundary.
    harness.send_text("/goal").await.unwrap();
    let outs = harness.drain_outputs(DRAIN).await;

    let goal = harness
        .goal_service
        .current(&session_id)
        .await
        .unwrap()
        .expect("goal row present");
    assert_eq!(goal.status, GoalStatus::BudgetLimited);
    assert!(
        notices(&outs).iter().any(|t| t.contains("budget reached")),
        "expected a budget-reached notice, got {:?}",
        notices(&outs)
    );
    let winddown_turns = harness
        .stub_llm
        .captured_requests()
        .iter()
        .filter(|r| {
            r.messages.iter().any(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text(t) if t.contains("This is your wind-down turn")),
                )
            })
        })
        .count();
    assert_eq!(winddown_turns, 1, "exactly one wind-down turn should fire");

    harness.shutdown().await;
}

/// When the global cost gate denies the next continuation call, the goal stops
/// as `SpendCapped` with a notice — it must not busy-loop retrying a hard cap.
#[tokio::test]
async fn goal_continuation_spend_capped_stops_with_notice() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    harness
        .stub_llm
        .push_stream_results(vec![Err(LlmError::GuardRejected(
            "daily limit exceeded".into(),
        ))]);

    harness.send_text("/goal ship the release").await.unwrap();
    let outs = harness.drain_outputs(DRAIN).await;

    let goal = harness
        .goal_service
        .current(&session_id)
        .await
        .unwrap()
        .expect("goal row present");
    assert_eq!(goal.status, GoalStatus::SpendCapped);
    assert!(
        notices(&outs).iter().any(|t| t.contains("spend limit")),
        "expected a spend-capped notice, got {:?}",
        notices(&outs)
    );

    harness.shutdown().await;
}

/// A transient continuation failure (a provider flake — not a cancel or the cost
/// gate) leaves the goal `Active` and emits no terminal notice: the loop backs
/// off and retries rather than dropping the objective.
#[tokio::test]
async fn goal_continuation_transient_error_keeps_goal_active() {
    let mut harness = AgentTestHarness::builder().build();
    let session_id = harness.session.id.clone();

    harness
        .stub_llm
        .push_stream_results(vec![Err(LlmError::Internal(anyhow::anyhow!(
            "provider flake"
        )))]);

    harness
        .send_text("/goal keep the build green")
        .await
        .unwrap();
    let outs = harness.drain_outputs(DRAIN).await;

    let goal = harness
        .goal_service
        .current(&session_id)
        .await
        .unwrap()
        .expect("goal row present");
    assert_eq!(
        goal.status,
        GoalStatus::Active,
        "a transient failure must not stop the goal"
    );
    let texts = notices(&outs);
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("complete") || t.contains("budget") || t.contains("spend limit")),
        "no terminal notice after a transient error: {texts:?}"
    );
    // The continuation turn did fire (the stub saw the framed steering).
    let fired = harness.stub_llm.captured_requests().iter().any(|r| {
        r.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text(t) if t.contains("continuing autonomous work")),
            )
        })
    });
    assert!(fired, "a continuation turn should have fired");

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
    harness.stub_llm.push_stream(stream_text("ok"));

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
