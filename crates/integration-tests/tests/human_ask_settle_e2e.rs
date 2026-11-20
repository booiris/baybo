//! The turn prelude and the Resume door, over a session a restart left with an
//! unapplied ask: the actor hydrates a transcript whose tail is a dangling
//! `tool_use` and a ledger row for it. Every door settles the ask before its
//! own append, so the result lands adjacent to the row that raised it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use baybo_agent::actor::{AgentMessage, CronDelivery};
use baybo_channels::{AgentEvent, AgentOutput};
use baybo_human_ask::test_support::MemoryHumanAskStore;
use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{ChatRequest, LlmResponse, StreamEvent, TokenUsage};
use baybo_model::approval::ApprovalRequest;
use baybo_model::human_ask::{
    AskId, AskKind, AskPhase, AskRequest, AskResolution, Question, QuestionAnswer, QuestionRequest,
};
use baybo_model::{ApprovalDecision, ChatMessage, ContentBlock, ResourceAccess, TriggerKind};
use baybo_session::SessionStore;
use baybo_store::HumanAskRow;
use baybo_tools::{Tool, ToolContext, ToolManifest, ToolOutput};
use baybo_turn::{TurnInput, TurnInputKind};
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::{Value, json};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const GATED_TOOL: &str = "gated_write";
const CALL_ID: &str = "call-parked";
const WRITTEN: &str = "written";

/// The ledger and ask id the probe reads once the fixture has raised the ask.
type ProbeTarget = Arc<Mutex<Option<(Arc<MemoryHumanAskStore>, AskId)>>>;

/// A gated write that records, when it runs, whether its ask was already
/// stamped applied.
struct ProbeWrite {
    runs: Arc<AtomicUsize>,
    store: ProbeTarget,
    applied_when_ran: Arc<Mutex<Option<bool>>>,
}

#[async_trait::async_trait]
impl Tool for ProbeWrite {
    fn name(&self) -> &str {
        GATED_TOOL
    }
    fn description(&self) -> String {
        "writes after asking".to_string()
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        vec![ResourceAccess::WriteFile {
            path: std::path::PathBuf::from("/tmp/human-ask-settle-e2e"),
        }]
    }
    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some((store, ask_id)) = self.store.lock().clone() {
            let applied = store
                .row(&ask_id)
                .is_some_and(|row| row.applied_at.is_some());
            *self.applied_when_ran.lock() = Some(applied);
        }
        Ok(ToolOutput::Text(WRITTEN.to_string()))
    }
}

fn manifest() -> ToolManifest {
    ToolManifest {
        name: GATED_TOOL.to_string(),
        description: "writes after asking".to_string(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: json!({"type": "object", "properties": {}}),
        capabilities: vec![baybo_tools::ToolCapability::WriteFile],
        channels: Vec::new(),
    }
}

struct Fixture {
    harness: AgentTestHarness,
    runs: Arc<AtomicUsize>,
    applied_when_ran: Arc<Mutex<Option<bool>>>,
    ask_id: AskId,
}

/// What a restart leaves behind: `[user, assistant(tool_use)]` on the
/// transcript, an ask row for the call, and the turn that raised it.
async fn restarted(tool: &str, kind: AskKind, resolution: Option<AskResolution>) -> Fixture {
    let runs = Arc::new(AtomicUsize::new(0));
    let store_slot = Arc::new(Mutex::new(None));
    let applied_when_ran = Arc::new(Mutex::new(None));
    let probe = Arc::new(ProbeWrite {
        runs: Arc::clone(&runs),
        store: Arc::clone(&store_slot),
        applied_when_ran: Arc::clone(&applied_when_ran),
    });
    let session = baybo_integration_tests::SessionBuilder::new().build();
    let ask_id = AskId::new();
    let request = match kind {
        AskKind::Approval => AskRequest::Approval(ApprovalRequest {
            ask_id: ask_id.clone(),
            tool_call_id: CALL_ID.to_string(),
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            tool: tool.to_string(),
            accesses: probe.accessed_resources(&json!({})),
            params_preview: "{}".to_string(),
            description: None,
        }),
        AskKind::Question => AskRequest::Question(QuestionRequest {
            questions: vec![Question {
                header: "Auth".to_string(),
                question: "Which auth?".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        }),
    };
    let now = Utc::now();
    let row = HumanAskRow {
        ask_id: ask_id.clone(),
        session_id: session.id.clone(),
        channel: session.channel.clone(),
        tool_call_id: CALL_ID.to_string(),
        turn_id: baybo_model::TurnId::new(),
        kind,
        phase: AskPhase::BeforeExecute,
        request,
        created_at: now,
        resolved_at: resolution.as_ref().map(|_| now),
        resolution,
        applied_at: None,
    };
    let transcript = vec![
        ChatMessage::system(vec![ContentBlock::Text(
            "You are Baybo, a test assistant.".to_string(),
        )]),
        ChatMessage::user(vec![ContentBlock::Text("write it".to_string())]),
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: CALL_ID.to_string(),
            name: tool.to_string(),
            input: json!({}),
            signature: None,
        }]),
    ];
    let harness = AgentTestHarness::builder()
        .session(session)
        .with_tool(probe, manifest())
        .with_channel_ask_gate()
        .with_seed_transcript(transcript)
        .with_seed_ask(row)
        .build();
    *store_slot.lock() = Some((Arc::clone(&harness.ask_store), ask_id.clone()));
    harness
        .turn_lifecycle
        .start_turn(
            harness.session.id.clone(),
            TriggerKind::User,
            TurnInput::UserChat {
                content: vec![ContentBlock::Text("write it".to_string())],
            },
            None,
        )
        .await
        .expect("seed the raising turn");
    Fixture {
        harness,
        runs,
        applied_when_ran,
        ask_id,
    }
}

async fn transcript(harness: &AgentTestHarness) -> Vec<ChatMessage> {
    harness
        .memory_session_store
        .load_session_messages_with_supersede(&harness.session.id)
        .await
        .expect("load transcript")
        .into_iter()
        .map(|row| row.message)
        .collect()
}

fn result_text(message: &ChatMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } if tool_use_id == CALL_ID => Some(content.to_string()),
        _ => None,
    })
}

/// The parked call's results, and the index of the one right after its
/// `tool_use` row (adjacency).
async fn adjacent_result(harness: &AgentTestHarness) -> String {
    let rows = transcript(harness).await;
    let results: Vec<String> = rows.iter().filter_map(result_text).collect();
    assert_eq!(
        results.len(),
        1,
        "exactly one result for the call: {rows:#?}"
    );
    let use_at = rows
        .iter()
        .position(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == CALL_ID))
        })
        .expect("the tool_use row is kept");
    result_text(&rows[use_at + 1]).unwrap_or_else(|| {
        panic!("the result must be adjacent to its tool_use: {rows:#?}");
    })
}

/// A strict provider: every `tool_use` is answered by the very next message.
fn assert_paired(request: &ChatRequest) {
    let messages = &request.messages;
    for (i, message) in messages.iter().enumerate() {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                let answered = messages.get(i + 1).is_some_and(|next| {
                    next.content.iter().any(|b| {
                        matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    })
                });
                assert!(
                    answered,
                    "tool_use {id} is not answered next: {messages:#?}"
                );
            }
        }
    }
}

async fn turns_of(harness: &AgentTestHarness, kind: TurnInputKind) -> usize {
    harness
        .turn_lifecycle
        .list_by_session(&harness.session.id, None)
        .await
        .expect("list turns")
        .iter()
        .filter(|turn| turn.input.input_kind() == kind)
        .count()
}

/// Turns of the chat Resume door: an answer carried on into a reply.
async fn resume_turns(harness: &AgentTestHarness) -> usize {
    turns_of(harness, TurnInputKind::Resume).await
}

/// Turns of a door's prelude: asks settled ahead of the door's own turn.
async fn settlement_turns(harness: &AgentTestHarness) -> usize {
    turns_of(harness, TurnInputKind::AskSettlement).await
}

fn reply(harness: &AgentTestHarness, text: &str) {
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text(text.to_string())]);
}

async fn stop(harness: AgentTestHarness) {
    tokio::time::timeout(STOP_TIMEOUT, harness.shutdown())
        .await
        .expect("actor stops");
}

#[tokio::test]
async fn a_new_message_abandons_a_dead_pending_ask_before_the_user_row() {
    let Fixture {
        mut harness,
        runs,
        ask_id,
        ..
    } = restarted(GATED_TOOL, AskKind::Approval, None).await;
    reply(&harness, "ok");
    harness.send_text("never mind").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let row = harness.ask_store.row(&ask_id).expect("row kept");
    assert_eq!(row.resolution, Some(AskResolution::Abandoned));
    assert!(row.applied_at.is_some());
    let result = adjacent_result(&harness).await;
    assert!(result.contains("withdrawn"), "{result}");
    assert!(
        !result.contains("interrupted by a crash"),
        "hydration must not fill a parked call: {result}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(settlement_turns(&harness).await, 1);
    assert_eq!(resume_turns(&harness).await, 0, "nothing was answered");
    let requests = harness.stub_llm.captured_requests();
    assert_eq!(requests.len(), 1);
    assert_paired(&requests[0]);
    stop(harness).await;
}

#[tokio::test]
async fn an_approval_answered_before_the_message_executes_once_before_the_user_row() {
    let Fixture {
        mut harness,
        runs,
        applied_when_ran,
        ask_id,
    } = restarted(
        GATED_TOOL,
        AskKind::Approval,
        Some(AskResolution::Approval(ApprovalDecision::Approve)),
    )
    .await;
    reply(&harness, "ok");
    harness.send_text("and then?").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the approved call runs once"
    );
    assert_eq!(
        *applied_when_ran.lock(),
        Some(true),
        "the ask is stamped applied before the call runs"
    );
    let result = adjacent_result(&harness).await;
    assert!(result.contains(WRITTEN), "{result}");
    assert!(
        harness
            .ask_store
            .row(&ask_id)
            .expect("row")
            .applied_at
            .is_some()
    );
    let requests = harness.stub_llm.captured_requests();
    assert_eq!(requests.len(), 1);
    assert_paired(&requests[0]);
    stop(harness).await;
}

#[tokio::test]
async fn a_denied_answer_appends_the_human_denial() {
    let Fixture {
        mut harness,
        runs,
        ask_id,
        ..
    } = restarted(
        GATED_TOOL,
        AskKind::Approval,
        Some(AskResolution::Approval(ApprovalDecision::Deny)),
    )
    .await;
    reply(&harness, "ok");
    harness.send_text("next").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&harness).await;
    assert!(result.contains("said no"), "{result}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(
        harness
            .ask_store
            .row(&ask_id)
            .expect("row")
            .applied_at
            .is_some()
    );
    stop(harness).await;
}

#[tokio::test]
async fn an_answered_question_lands_as_the_ask_user_result() {
    let Fixture {
        mut harness,
        ask_id,
        ..
    } = restarted(
        baybo_model::human_ask::ASK_USER_TOOL_NAME,
        AskKind::Question,
        Some(AskResolution::Answered(vec![QuestionAnswer {
            header: "Auth".to_string(),
            selected: vec!["JWT".to_string()],
            other: None,
        }])),
    )
    .await;
    reply(&harness, "ok");
    harness.send_text("next").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&harness).await;
    assert!(result.contains("JWT"), "{result}");
    assert!(
        harness
            .ask_store
            .row(&ask_id)
            .expect("row")
            .applied_at
            .is_some()
    );
    stop(harness).await;
}

#[tokio::test]
async fn a_cancelled_question_keeps_its_result_after_restart() {
    let Fixture { mut harness, .. } = restarted(
        baybo_model::human_ask::ASK_USER_TOOL_NAME,
        AskKind::Question,
        Some(AskResolution::Cancelled),
    )
    .await;
    reply(&harness, "ok");
    harness.send_text("next").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;
    let result = adjacent_result(&harness).await;
    assert!(result.contains(r#"{"cancelled":true}"#), "{result}");
    stop(harness).await;
}

#[tokio::test]
async fn an_abandoned_question_reads_declined() {
    let Fixture { mut harness, .. } = restarted(
        baybo_model::human_ask::ASK_USER_TOOL_NAME,
        AskKind::Question,
        None,
    )
    .await;
    reply(&harness, "ok");
    harness.send_text("next").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&harness).await;
    assert!(result.contains("moved on without answering"), "{result}");
    stop(harness).await;
}

#[tokio::test]
async fn a_batch_of_messages_settles_first() {
    let Fixture { mut harness, .. } = restarted(GATED_TOOL, AskKind::Approval, None).await;
    reply(&harness, "ok");
    let message = |text: &str| baybo_channels::IncomingMessage {
        message: baybo_channels::Message {
            id: text.to_string(),
            session_id: harness.session.id.clone(),
            channel: harness.session.channel.clone(),
            sender: harness.session.user.clone(),
            content: vec![ContentBlock::Text(text.to_string())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: Default::default(),
        },
        platform_msg_id: String::new(),
        bot_id: String::new(),
    };
    let batch = vec![message("one"), message("two")];
    harness
        .mailbox
        .send(AgentMessage::UserInputBatch(batch))
        .await
        .expect("send batch");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(adjacent_result(&harness).await.contains("withdrawn"));
    assert_paired(&harness.stub_llm.captured_requests()[0]);
    stop(harness).await;
}

#[tokio::test]
async fn a_cron_fire_settles_before_its_prompt() {
    let Fixture { mut harness, .. } = restarted(GATED_TOOL, AskKind::Approval, None).await;
    reply(&harness, "ok");
    harness
        .mailbox
        .send(AgentMessage::CronTrigger {
            job_id: "job-1".to_string(),
            title: "job".to_string(),
            prompt: "do the thing".to_string(),
            delivery: CronDelivery::Channel,
            builtin: None,
        })
        .await
        .expect("send cron");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(adjacent_result(&harness).await.contains("withdrawn"));
    assert_paired(&harness.stub_llm.captured_requests()[0]);
    stop(harness).await;
}

#[tokio::test]
async fn an_issue_run_settles_before_its_brief() {
    let Fixture {
        mut harness, runs, ..
    } = restarted(
        GATED_TOOL,
        AskKind::Approval,
        Some(AskResolution::Approval(ApprovalDecision::Approve)),
    )
    .await;
    // An issue run is nobody's live chat: its LLM call does not stream.
    harness.stub_llm.push_response(LlmResponse {
        content: "ok".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: None,
    });
    harness
        .mailbox
        .send(AgentMessage::IssueRun {
            run_id: baybo_model::IssueRunId::generate(),
            number: 1,
            brief: "fix it".to_string(),
            files: Vec::new(),
            checkout: "/tmp".to_string(),
        })
        .await
        .expect("send issue run");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(adjacent_result(&harness).await.contains(WRITTEN));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "executes once, under the Resume turn"
    );
    assert_paired(&harness.stub_llm.captured_requests()[0]);
    stop(harness).await;
}

#[tokio::test]
async fn compact_settles_before_the_summariser_reads_the_transcript() {
    let Fixture { mut harness, .. } = restarted(GATED_TOOL, AskKind::Approval, None).await;
    harness.stub_llm.push_response(LlmResponse {
        content: "summary of the conversation".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: None,
    });
    harness.send_text("/compact").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(adjacent_result(&harness).await.contains("withdrawn"));
    assert_eq!(settlement_turns(&harness).await, 1);
    stop(harness).await;
}

#[tokio::test]
async fn the_resume_door_applies_the_answer_and_carries_on() {
    let Fixture {
        mut harness,
        runs,
        ask_id,
        ..
    } = restarted(
        GATED_TOOL,
        AskKind::Approval,
        Some(AskResolution::Approval(ApprovalDecision::Approve)),
    )
    .await;
    reply(&harness, "all done");
    harness
        .mailbox
        .send(AgentMessage::ResumeAsk {
            ask_id: ask_id.clone(),
        })
        .await
        .expect("send resume");
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(adjacent_result(&harness).await.contains(WRITTEN));
    assert_eq!(
        resume_turns(&harness).await,
        1,
        "one Resume turn, continued"
    );
    let requests = harness.stub_llm.captured_requests();
    assert_eq!(requests.len(), 1);
    assert_paired(&requests[0]);
    assert!(
        outputs.iter().any(|o: &AgentOutput| matches!(
            &o.event,
            AgentEvent::Message(m) if m.content.iter().any(|b| matches!(b, ContentBlock::Text(t) if t.contains("all done")))
        )),
        "the reply goes out: {outputs:#?}"
    );
    stop(harness).await;
}

#[tokio::test]
async fn the_resume_door_does_nothing_for_an_applied_ask() {
    let Fixture {
        mut harness,
        runs,
        ask_id,
        ..
    } = restarted(
        GATED_TOOL,
        AskKind::Approval,
        Some(AskResolution::Approval(ApprovalDecision::Approve)),
    )
    .await;
    reply(&harness, "first");
    harness.send_text("hello").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    harness
        .mailbox
        .send(AgentMessage::ResumeAsk { ask_id })
        .await
        .expect("send resume");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "never executes twice");
    assert_eq!(
        harness.stub_llm.captured_requests().len(),
        1,
        "no second LLM call"
    );
    assert_eq!(settlement_turns(&harness).await, 1, "the prelude's turn");
    assert_eq!(
        resume_turns(&harness).await,
        0,
        "and no Resume turn after it"
    );
    stop(harness).await;
}

#[tokio::test]
async fn a_session_with_nothing_unapplied_opens_no_resume_turn() {
    let mut harness = AgentTestHarness::builder().build();
    reply(&harness, "hi");
    harness.send_text("hello").await.expect("send");
    harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(resume_turns(&harness).await, 0);
    assert_eq!(settlement_turns(&harness).await, 0);
    stop(harness).await;
}

#[tokio::test(start_paused = true)]
async fn a_background_notification_settles_before_its_prompt() {
    let Fixture { mut harness, .. } = restarted(GATED_TOOL, AskKind::Approval, None).await;
    reply(&harness, "noted");
    harness
        .mailbox
        .send(AgentMessage::BackgroundJobFinished(Box::new(
            baybo_model::PendingBackgroundResult::subagent(
                "bg-1",
                "explorer",
                "find it",
                baybo_model::SessionId::from("child-1"),
                "found it",
                baybo_model::SubagentExitStatus::Completed,
            ),
        )))
        .await
        .expect("inject BackgroundJobFinished");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(adjacent_result(&harness).await.contains("withdrawn"));
    let requests = harness.stub_llm.captured_requests();
    assert_eq!(requests.len(), 1, "the notification turn ran");
    assert_paired(&requests[0]);
    stop(harness).await;
}

#[tokio::test]
async fn skill_env_approval_resumes_without_reprompting_and_does_not_cache() {
    assert_skill_env_resume(false).await;
}

#[tokio::test]
async fn skill_env_resume_refuses_expanded_requirements() {
    assert_skill_env_resume(true).await;
}

async fn assert_skill_env_resume(expanded: bool) {
    use baybo_skills::{AlwaysPass, SkillDefinition, SkillRegistry, SkillRequirements};
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("SKILL.md"), "# Secret Skill Body\n").expect("write skill");
    let skill_registry = Arc::new(SkillRegistry::new());
    skill_registry.register(SkillDefinition {
        name: "env-skill".to_string(),
        version: "1.0.0".to_string(),
        description: "needs PATH".to_string(),
        command: Some("env-skill".to_string()),
        agent_invocable: true,
        channels: vec![],
        argument_hint: None,
        prompt_template: "# Secret Skill Body\n".to_string(),
        allowed_tools: vec![],
        source: baybo_model::ArtifactSource::Workspace,
        trust_level: baybo_model::TrustLevel::Trusted,
        requirements: SkillRequirements {
            required_bins: vec![],
            required_env: if expanded {
                vec!["PATH".into(), "HOME".into()]
            } else {
                vec!["PATH".into()]
            },
            required_models: vec![],
        },
        token_budget_hint: 0,
        source_path: Some(dir.path().to_path_buf()),
        linked_files: Default::default(),
    });
    let (skill_tool, skill_manifest) =
        baybo_skills::tools::build(skill_registry, Arc::new(AlwaysPass));

    let session = baybo_integration_tests::SessionBuilder::new().build();
    let ask_id = AskId::new();
    let now = Utc::now();
    let row = HumanAskRow {
        ask_id: ask_id.clone(),
        session_id: session.id.clone(),
        channel: session.channel.clone(),
        tool_call_id: CALL_ID.to_string(),
        turn_id: baybo_model::TurnId::new(),
        kind: AskKind::Approval,
        phase: AskPhase::BeforeExecute,
        request: AskRequest::Approval(ApprovalRequest {
            ask_id: ask_id.clone(),
            tool_call_id: CALL_ID.to_string(),
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            tool: baybo_skills::tools::SKILL_TOOL_NAME.to_string(),
            accesses: vec![ResourceAccess::Env {
                vars: vec!["PATH".to_string()],
            }],
            params_preview: "Skill 'env-skill' env vars: PATH".to_string(),
            description: None,
        }),
        created_at: now,
        resolved_at: Some(now),
        resolution: Some(AskResolution::Approval(ApprovalDecision::ApproveAlways)),
        applied_at: None,
    };
    let transcript = vec![
        ChatMessage::system(vec![ContentBlock::Text(
            "You are Baybo, a test assistant.".to_string(),
        )]),
        ChatMessage::user(vec![ContentBlock::Text("load env-skill".to_string())]),
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: CALL_ID.to_string(),
            name: baybo_skills::tools::SKILL_TOOL_NAME.to_string(),
            input: json!({ "skill": "env-skill" }),
            signature: None,
        }]),
    ];

    let mut harness = AgentTestHarness::builder()
        .session(session)
        .with_tool(skill_tool, skill_manifest)
        .with_channel_ask_gate()
        .with_seed_transcript(transcript)
        .with_seed_ask(row)
        .build();
    harness
        .turn_lifecycle
        .start_turn(
            harness.session.id.clone(),
            TriggerKind::User,
            TurnInput::UserChat {
                content: vec![ContentBlock::Text("load env-skill".to_string())],
            },
            None,
        )
        .await
        .expect("seed raising turn");

    reply(&harness, "loaded env-skill");
    harness
        .mailbox
        .send(AgentMessage::ResumeAsk {
            ask_id: ask_id.clone(),
        })
        .await
        .expect("send resume");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&harness).await;
    if expanded {
        assert!(!result.contains("Secret Skill Body"));
        assert!(result.contains("approval requirements changed"), "{result}");
    } else {
        assert!(result.contains("Secret Skill Body"), "{result}");
    }
    assert_eq!(
        harness.ask_store.rows().len(),
        1,
        "no second ask raised during resume"
    );
    let stored_session = harness
        .session_store
        .get(&harness.session.id)
        .await
        .expect("load session")
        .expect("session exists");
    assert!(
        stored_session.state.approved_resources.is_empty(),
        "ApproveAlways on resumed uncached Skill env must not persist to session cache"
    );
    stop(harness).await;
}

#[tokio::test]
async fn during_execute_unsandboxed_retry_resumes_straight_to_unsandboxed_and_does_not_cache() {
    use baybo_tools::builtin::bash::BashTool;
    use baybo_tools::builtin::permission::{LivePermissionMode, PermissionMode};

    let home = tempfile::tempdir().expect("tempdir");
    let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
    std::fs::create_dir_all(paths.work_dir()).expect("work dir");

    // Manual mode proves Resume dispatches `DuringExecuteResumer::resume_during_execute`
    // (skipping `pre_execute_ask`) rather than re-running the normal pre-execute gate.
    let bash = Arc::new(
        BashTool::new(paths, baybo_process::ProcessManager::transient())
            .with_permission_handle(Arc::new(LivePermissionMode::new(PermissionMode::Manual))),
    );
    let bash_manifest = ToolManifest {
        name: "Bash".to_string(),
        description: "bash".to_string(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: json!({"type": "object", "properties": {}}),
        capabilities: vec![baybo_tools::ToolCapability::ExecCommand],
        channels: Vec::new(),
    };

    let session = baybo_integration_tests::SessionBuilder::new().build();
    let ask_id = AskId::new();
    let now = Utc::now();
    let cmd = "printf 'unsandboxed-resume-ok\\n'";
    let row = HumanAskRow {
        ask_id: ask_id.clone(),
        session_id: session.id.clone(),
        channel: session.channel.clone(),
        tool_call_id: CALL_ID.to_string(),
        turn_id: baybo_model::TurnId::new(),
        kind: AskKind::Approval,
        phase: AskPhase::DuringExecuteResumable,
        request: AskRequest::Approval(ApprovalRequest {
            ask_id: ask_id.clone(),
            tool_call_id: CALL_ID.to_string(),
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            tool: "Bash".to_string(),
            accesses: vec![ResourceAccess::ExecCommand {
                command: cmd.to_string(),
            }],
            params_preview: "Sandboxed Bash failed".to_string(),
            description: None,
        }),
        created_at: now,
        resolved_at: Some(now),
        resolution: Some(AskResolution::Approval(ApprovalDecision::ApproveAlways)),
        applied_at: None,
    };
    let transcript = vec![
        ChatMessage::system(vec![ContentBlock::Text(
            "You are Baybo, a test assistant.".to_string(),
        )]),
        ChatMessage::user(vec![ContentBlock::Text("run command".to_string())]),
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: CALL_ID.to_string(),
            name: "Bash".to_string(),
            input: json!({ "command": cmd }),
            signature: None,
        }]),
    ];

    let mut harness = AgentTestHarness::builder()
        .session(session)
        .with_tool(bash, bash_manifest)
        .with_channel_ask_gate()
        .with_seed_transcript(transcript)
        .with_seed_ask(row)
        .build();
    harness
        .turn_lifecycle
        .start_turn(
            harness.session.id.clone(),
            TriggerKind::User,
            TurnInput::UserChat {
                content: vec![ContentBlock::Text("run command".to_string())],
            },
            None,
        )
        .await
        .expect("seed raising turn");

    reply(&harness, "ran unsandboxed");
    harness
        .mailbox
        .send(AgentMessage::ResumeAsk {
            ask_id: ask_id.clone(),
        })
        .await
        .expect("send resume");
    harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&harness).await;
    assert!(
        result.contains("unsandboxed-resume-ok") && result.contains("sandbox_escalation"),
        "resumed DuringExecuteResumable approval must execute unsandboxed with escalation note: {result}"
    );
    assert_eq!(
        harness.ask_store.rows().len(),
        1,
        "no second ask raised during resume"
    );
    let stored_session = harness
        .session_store
        .get(&harness.session.id)
        .await
        .expect("load session")
        .expect("session exists");
    assert!(
        stored_session.state.approved_resources.is_empty(),
        "ApproveAlways on resumed unsandboxed retry must not persist to session cache"
    );
    stop(harness).await;
}
