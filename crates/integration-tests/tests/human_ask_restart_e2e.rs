//! Durable approvals across a real restart: one harness raises an approval
//! over a sqlite database and is shut down the way the gateway shuts down
//! (the actors' parent token trips); a second one is built on the same
//! database file, with the gateway's channels and push relay installed over
//! its ask queues before the boot reload, as `build_managers` orders them.
//! The wired LLM is a strict provider: any request whose `tool_use` is not
//! answered by the very next message is refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use baybo_channels::{AgentEvent, AgentOutput, Channel, ChannelRegistry};
use baybo_config::ChannelsConfig;
use baybo_gateway::channel::boot;
use baybo_gateway::push::ask::{AskPushRelay, AskPushStream};
use baybo_human_ask::AskQueueEvent;
use baybo_integration_tests::{AgentTestHarness, AgentTestHarnessBuilder, SessionBuilder};
use baybo_llm::{LlmResponse, StreamEvent, TokenUsage, ToolCallInfo};
use baybo_model::human_ask::{AskId, AskResolution};
use baybo_model::{
    ApprovalDecision, ChannelType, ChatMessage, ContentBlock, ResourceAccess, Session,
    TriggerSource,
};
use baybo_storage::Store;
use baybo_tools::{Tool, ToolContext, ToolManifest, ToolOutput};
use baybo_turn::TurnInputKind;
use parking_lot::Mutex;
use serde_json::{Value, json};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const GATED_TOOL: &str = "gated_write";
const CALL_ID: &str = "call-restart";
const WRITTEN: &str = "written";
const DB_FILE: &str = "baybo.db";

/// Asks to write a file before it runs; counts how often it actually ran.
struct GatedWrite {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for GatedWrite {
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
            path: PathBuf::from("/tmp/human-ask-restart-e2e"),
        }]
    }
    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
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

/// One boot of the process over `db`: the agent side, plus the gateway's
/// owner channel and push relay over the same ask queues.
struct Node {
    harness: AgentTestHarness,
    owner: Arc<Channel>,
    pushes: AskPushStream,
    closed: Arc<Mutex<Vec<AskQueueEvent>>>,
    _registry: Arc<ChannelRegistry>,
}

fn builder(session: &Session, store: Store, runs: &Arc<AtomicUsize>) -> AgentTestHarnessBuilder {
    AgentTestHarness::builder()
        .session(session.clone())
        .with_store(store)
        .with_tool(
            Arc::new(GatedWrite {
                runs: Arc::clone(runs),
            }),
            manifest(),
        )
        .with_channel_ask_gate()
        .with_strict_tool_pairing()
}

/// Boot a node: open the database, build the agent, install the gateway's
/// channels and push relay, and only then run the boot reload.
async fn boot(
    db: &Path,
    session: &Session,
    runs: &Arc<AtomicUsize>,
    resume_door: bool,
    before_reload: impl FnOnce(&AgentTestHarness),
) -> Node {
    let store = Store::open(db).await.expect("open the database");
    let mut b = builder(session, store, runs);
    if resume_door {
        b = b.with_resume_door();
    }
    let harness = b.build();
    let registry = Arc::new(ChannelRegistry::new());
    boot::install_channels(
        &registry,
        &ChannelsConfig::default(),
        &harness.ask_core,
        &harness.ask_queues,
    )
    .expect("install channels");
    let owner = registry.get(&ChannelType::owner()).expect("owner channel");
    let (relay, pushes) = AskPushRelay::new();
    relay.install(&owner);
    let closed = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&closed);
    owner
        .ask_queue()
        .expect("owner queue")
        .add_ask_watcher(Arc::new(move |event| {
            if matches!(event, AskQueueEvent::AskClosed { .. }) {
                sink.lock().push(event);
            }
        }));
    before_reload(&harness);
    harness
        .human_asks
        .reload_pending()
        .await
        .expect("reload pending asks");
    Node {
        harness,
        owner,
        pushes,
        closed,
        _registry: registry,
    }
}

fn owner_session(trigger: TriggerSource) -> Session {
    let mut session = SessionBuilder::new()
        .id(format!("sess-{}", AskId::new()))
        .channel(ChannelType::owner())
        .build();
    session.trigger = trigger;
    session
}

/// The first boot: a chat turn parks its one call on a live approval, then
/// the process shuts down — the actors' parent token trips and the actor is
/// awaited to quiescence (its turn torn down), not to process exit.
async fn raise_then_shut_down(db: &Path, session: &Session, runs: &Arc<AtomicUsize>) -> AskId {
    let store = Store::open(db).await.expect("open the database");
    store.session.save(session).await.expect("save the session");
    let mut harness = builder(session, store, runs).build();
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
            id: CALL_ID.to_string(),
            name: GATED_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        })]);
    harness.send_text("write it").await.expect("send");
    let queue = harness.ask_queues.get_or_create(&session.channel);
    tokio::time::timeout(WAIT_TIMEOUT, async {
        while queue.is_empty() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the call parks on an approval");
    let rows = harness
        .ask_ledger
        .list_unapplied_for_session(&session.id)
        .await
        .expect("list asks");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let ask_id = rows[0].ask_id.clone();

    harness.actor_parent_token.cancel();
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(
        completed_for_call(&outputs),
        0,
        "shutdown emits nothing for the parked call"
    );
    assert_eq!(harness.strict_rejections.load(Ordering::SeqCst), 0);
    tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
        .await
        .expect("the actor quiesces");
    ask_id
}

fn completed_for_call(outputs: &[AgentOutput]) -> usize {
    outputs
        .iter()
        .filter(
            |o| matches!(&o.event, AgentEvent::ToolCompleted { call_id, .. } if call_id == CALL_ID),
        )
        .count()
}

async fn transcript(harness: &AgentTestHarness) -> Vec<ChatMessage> {
    harness
        .session_store
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

fn tool_use_at(rows: &[ChatMessage]) -> Option<usize> {
    rows.iter().position(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == CALL_ID))
    })
}

/// The call's single result, which must sit right after its `tool_use`.
async fn adjacent_result(harness: &AgentTestHarness) -> String {
    let rows = transcript(harness).await;
    let results: Vec<String> = rows.iter().filter_map(result_text).collect();
    assert_eq!(results.len(), 1, "exactly one result: {rows:#?}");
    let at = tool_use_at(&rows).expect("the tool_use row is kept");
    let result = result_text(&rows[at + 1])
        .unwrap_or_else(|| panic!("the result must be adjacent to its tool_use: {rows:#?}"));
    assert!(
        !result.contains("interrupted"),
        "a parked call is never crash-filled: {result}"
    );
    result
}

async fn resume_turns(harness: &AgentTestHarness) -> usize {
    harness
        .turn_lifecycle
        .list_by_session(&harness.session.id, None)
        .await
        .expect("list turns")
        .iter()
        .filter(|turn| turn.input.input_kind() == TurnInputKind::Resume)
        .count()
}

fn reply(harness: &AgentTestHarness, text: &str) {
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text(text.to_string())]);
}

fn replied(outputs: &[AgentOutput], text: &str) -> bool {
    outputs.iter().any(|o| {
        matches!(
            &o.event,
            AgentEvent::Message(m) if m.content.iter().any(|b| matches!(b, ContentBlock::Text(t) if t.contains(text)))
        )
    })
}

async fn row(harness: &AgentTestHarness, ask_id: &AskId) -> baybo_store::HumanAskRow {
    harness
        .ask_ledger
        .get(ask_id)
        .await
        .expect("read the ask")
        .expect("the ask row is never deleted")
}

async fn no_more_pushes(pushes: &mut AskPushStream) {
    assert!(
        tokio::time::timeout(DRAIN_TIMEOUT, pushes.recv())
            .await
            .is_err(),
        "no further push"
    );
}

async fn stop(node: Node) {
    tokio::time::timeout(WAIT_TIMEOUT, node.harness.shutdown())
        .await
        .expect("actor stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_survives_a_restart_and_its_answer_resumes_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;

    let cards = node.owner.pending_approvals(&session.id);
    assert_eq!(cards.len(), 1, "SubscribeState carries the card");
    assert_eq!(cards[0].call_id, ask_id.to_string());
    assert_eq!(cards[0].tool_call_id, CALL_ID);
    assert!(
        node.owner.pending_approval_sessions().contains(&session.id),
        "the chat list reads awaiting_human"
    );
    tokio::time::timeout(WAIT_TIMEOUT, node.pushes.recv())
        .await
        .expect("the unsubscribed session is pushed")
        .expect("push stream open");
    no_more_pushes(&mut node.pushes).await;
    let rows = transcript(&node.harness).await;
    assert!(tool_use_at(&rows).is_some(), "{rows:#?}");
    assert!(
        rows.iter().all(|m| result_text(m).is_none()),
        "the tool_use stays dangling: {rows:#?}"
    );
    assert!(row(&node.harness, &ask_id).await.is_pending());

    reply(&node.harness, "all done");
    node.harness
        .human_asks
        .resolve(&ask_id, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer the dead ask");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "executed exactly once");
    assert!(adjacent_result(&node.harness).await.contains(WRITTEN));
    assert_eq!(resume_turns(&node.harness).await, 1);
    assert!(replied(&outputs, "all done"), "{outputs:#?}");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert_eq!(node.harness.stub_llm.captured_requests().len(), 1);
    assert!(row(&node.harness, &ask_id).await.applied_at.is_some());
    assert!(node.owner.pending_approvals(&session.id).is_empty());
    assert!(
        node.closed.lock().iter().any(|e| matches!(
            e,
            AskQueueEvent::AskClosed { ask_id: closed, outcome: AskResolution::Approval(ApprovalDecision::Approve), .. } if *closed == ask_id
        )),
        "AskResolved goes out for the answer"
    );
    stop(node).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_claimed_before_a_crash_is_redriven_by_the_next_boot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    // The claim lands, then the process dies before the Resume turn runs: no
    // Resume door is wired, so nothing applies the answer on this boot.
    let node = boot(&db, &session, &runs, false, |_| {}).await;
    node.harness
        .human_asks
        .resolve(&ask_id, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer the dead ask");
    let claimed = row(&node.harness, &ask_id).await;
    assert!(claimed.resolved_at.is_some() && claimed.applied_at.is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    stop(node).await;

    let mut node = boot(&db, &session, &runs, true, |h| reply(h, "carried on")).await;
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "executed exactly once");
    assert!(adjacent_result(&node.harness).await.contains(WRITTEN));
    assert!(replied(&outputs, "carried on"), "{outputs:#?}");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert!(row(&node.harness, &ask_id).await.applied_at.is_some());
    assert!(
        node.owner.pending_approvals(&session.id).is_empty(),
        "an answered ask is not re-shown"
    );
    no_more_pushes(&mut node.pushes).await;
    stop(node).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_message_after_a_restart_abandons_the_dead_ask_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    reply(&node.harness, "ok");
    node.harness.send_text("never mind").await.expect("send");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = row(&node.harness, &ask_id).await;
    assert_eq!(after.resolution, Some(AskResolution::Abandoned));
    assert!(after.applied_at.is_some());
    assert!(adjacent_result(&node.harness).await.contains("torn down"));
    assert!(replied(&outputs, "ok"), "{outputs:#?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert!(node.owner.pending_approvals(&session.id).is_empty());
    assert!(
        node.closed.lock().iter().any(|e| matches!(
            e,
            AskQueueEvent::AskClosed { ask_id: closed, outcome: AskResolution::Abandoned, .. } if *closed == ask_id
        )),
        "AskResolved goes out so open tabs drop the card"
    );
    stop(node).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_client_answering_before_the_prelude_wins_over_the_new_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    // Without a Resume door the answer's own turn never runs, so the claim
    // stands between the message arriving and the prelude — the race.
    let mut node = boot(&db, &session, &runs, false, |_| {}).await;
    node.harness
        .human_asks
        .resolve(&ask_id, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("the other tab answers");
    reply(&node.harness, "ok");
    node.harness.send_text("and now?").await.expect("send");
    node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = row(&node.harness, &ask_id).await;
    assert_eq!(
        after.resolution,
        Some(AskResolution::Approval(ApprovalDecision::Approve))
    );
    assert!(after.applied_at.is_some());
    assert!(adjacent_result(&node.harness).await.contains(WRITTEN));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the real answer lands once");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    stop(node).await;
}

fn issue_trigger() -> TriggerSource {
    TriggerSource::Issue {
        project_id: baybo_model::ProjectId::generate(),
        issue_id: baybo_model::IssueId::generate(),
        number: 1,
    }
}

/// An issue run is nobody's live chat: its LLM calls do not stream.
fn run_reply(harness: &AgentTestHarness, text: &str, tool_calls: Vec<ToolCallInfo>) {
    harness.stub_llm.push_response(LlmResponse {
        content: text.to_string(),
        content_blocks: Vec::new(),
        tool_calls,
        usage: TokenUsage::default(),
        thinking: None,
    });
}

async fn requeue_issue_run(harness: &AgentTestHarness) {
    harness
        .mailbox
        .send(baybo_agent::actor::AgentMessage::IssueRun {
            run_id: baybo_model::IssueRunId::generate(),
            number: 1,
            brief: "fix it".to_string(),
            files: Vec::new(),
            checkout: "/nonexistent-checkout".to_string(),
        })
        .await
        .expect("send the requeued run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeued_issue_run_applies_an_answer_given_before_it_started() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(issue_trigger());
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    node.harness
        .human_asks
        .resolve(&ask_id, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer from the card");
    node.harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "an issue session is not resumed by the answer; its requeued run applies it"
    );
    assert_eq!(resume_turns(&node.harness).await, 0);

    run_reply(&node.harness, "run done", Vec::new());
    requeue_issue_run(&node.harness).await;
    node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "executed exactly once");
    assert!(adjacent_result(&node.harness).await.contains(WRITTEN));
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert!(row(&node.harness, &ask_id).await.applied_at.is_some());
    assert_eq!(
        node.harness.stub_llm.captured_requests().len(),
        1,
        "the run's one LLM call sees the settled result"
    );
    stop(node).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeued_issue_run_re_asks_live_when_nobody_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(issue_trigger());
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    const AGAIN: &str = "call-again";
    run_reply(
        &node.harness,
        "",
        vec![ToolCallInfo {
            id: AGAIN.to_string(),
            name: GATED_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        }],
    );
    run_reply(&node.harness, "run done", Vec::new());
    requeue_issue_run(&node.harness).await;

    let queue = node.owner.ask_queue().expect("owner queue").clone();
    let live = tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            if let Some(ask) = queue
                .list()
                .into_iter()
                .find(|a| a.tool_call_id == AGAIN && queue.liveness(&a.ask_id) == Some(true))
            {
                return ask;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the run re-asks live");
    let abandoned = row(&node.harness, &ask_id).await;
    assert_eq!(abandoned.resolution, Some(AskResolution::Abandoned));
    assert!(adjacent_result(&node.harness).await.contains("torn down"));

    node.harness
        .human_asks
        .resolve(
            &live.ask_id,
            AskResolution::Approval(ApprovalDecision::Approve),
        )
        .await
        .expect("answer the live ask");
    node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "executed exactly once");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert!(row(&node.harness, &live.ask_id).await.applied_at.is_some());
    assert_eq!(node.harness.stub_llm.captured_requests().len(), 2);
    stop(node).await;
}
