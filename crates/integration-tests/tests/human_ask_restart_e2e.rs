//! Durable approvals across a real restart: one harness raises an approval
//! over a sqlite database and is shut down the way the gateway shuts down
//! (the actors' parent token trips); a second one is built on the same
//! database file, with the gateway's channels and push relay installed over
//! its ask queues before the boot reload, as `build_managers` orders them.
//! The wired LLM is a strict provider: any request whose `tool_use` is not
//! answered by the very next message is refused.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use baybo_channels::{AgentEvent, AgentOutput, Channel, ChannelRegistry};
use baybo_config::ChannelsConfig;
use baybo_gateway::channel::boot;
use baybo_gateway::push::ask::{AskPushRelay, AskPushStream};
use baybo_human_ask::AskQueueEvent;
use baybo_integration_tests::human_ask::{
    CALL_ID, GATED_TOOL, GatedWrite, WRITTEN, adjacent_result, reply, result_text, resume_turns,
    tool_use_at, transcript,
};
use baybo_integration_tests::{AgentTestHarness, AgentTestHarnessBuilder, SessionBuilder};
use baybo_llm::{LlmResponse, StreamEvent, TokenUsage, ToolCallInfo};
use baybo_model::human_ask::{AskId, AskResolution};
use baybo_model::{
    ApprovalDecision, ChannelType, ChatMessage, ContentBlock, Session, TriggerSource,
};
use baybo_storage::Store;
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolManifest, ToolOutput};
use parking_lot::Mutex;
use serde_json::{Value, json};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const DB_FILE: &str = "baybo.db";

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
    let tool = GatedWrite::new(Arc::clone(runs));
    let manifest = tool.manifest();
    AgentTestHarness::builder()
        .session(session.clone())
        .with_store(store)
        .with_tool(Arc::new(tool), manifest)
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
    let trace_store = Arc::clone(&store.trace);
    let mut b = builder(session, store, runs);
    if resume_door {
        b = b.with_resume_door();
    }
    let harness = b.build();
    baybo_agent::recovery::recover_orphaned_traces_and_turns(
        trace_store,
        Arc::clone(&harness.turn_lifecycle),
    )
    .await;
    let registry = Arc::new(ChannelRegistry::new());
    boot::install_channels(
        &registry,
        &ChannelsConfig::default(),
        &baybo_human_ask::AskChannels::new(
            Arc::clone(&harness.ask_core),
            harness.ask_queues.clone(),
        ),
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
    assert_eq!(cards[0].ask_id, ask_id);
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

/// The idle reaper's eviction, without a restart: the parked turn is ended
/// the way a shutdown ends it, the card stays answerable in the same
/// process, and the answer resumes the turn through the Resume door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_released_parked_turn_is_resumed_by_its_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join(DB_FILE))
        .await
        .expect("open the database");
    let session = owner_session(TriggerSource::User);
    store
        .session
        .save(&session)
        .await
        .expect("save the session");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut harness = builder(&session, store, &runs).with_resume_door().build();
    call_gated_write(&harness);
    harness.send_text("write it").await.expect("send");
    let ask_id = parked_ask(&harness).await;
    let queue = harness.ask_queues.get_or_create(&session.channel);

    assert_eq!(reap_idle(&harness).await, 1);
    tokio::time::timeout(WAIT_TIMEOUT, async {
        while !harness.supervisor.is_empty() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the released actor exits before the answer");
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(
        completed_for_call(&outputs),
        0,
        "the release appends nothing"
    );
    assert_eq!(
        queue.liveness(&ask_id),
        Some(false),
        "the card stays, with no waiter behind it"
    );
    let parked = row(&harness, &ask_id).await;
    assert!(parked.is_pending());
    let original = harness
        .turn_lifecycle
        .get(&parked.turn_id)
        .await
        .expect("read turn")
        .expect("raising turn");
    assert!(matches!(
        original.status,
        baybo_turn::TurnStatus::Cancelled { .. }
    ));
    assert!(
        transcript(&harness)
            .await
            .iter()
            .all(|row| result_text(row).is_none())
    );
    assert_eq!(resume_turns(&harness).await, 0);

    reply(&harness, "all done");
    harness
        .human_asks
        .resolve(&ask_id, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer the released ask");
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(
        harness.supervisor.registered_session_ids(),
        vec![session.id.clone()]
    );

    assert_eq!(runs.load(Ordering::SeqCst), 1, "executed exactly once");
    assert!(adjacent_result(&harness).await.contains(WRITTEN));
    assert_eq!(resume_turns(&harness).await, 1);
    assert!(replied(&outputs, "all done"), "{outputs:#?}");
    assert_eq!(harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert!(row(&harness, &ask_id).await.applied_at.is_some());
    tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
        .await
        .expect("the actor quiesces");
}

/// A message the user sent while the approval was up is still in the mailbox
/// when the reaper releases the turn. A release is not a `/stop`: the
/// message must run as the next turn, whose prelude withdraws the ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_queued_behind_a_released_turn_still_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join(DB_FILE))
        .await
        .expect("open the database");
    let session = owner_session(TriggerSource::User);
    store
        .session
        .save(&session)
        .await
        .expect("save the session");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut harness = builder(&session, store, &runs).with_resume_door().build();
    call_gated_write(&harness);
    harness.send_text("write it").await.expect("send");
    let ask_id = parked_ask(&harness).await;

    harness.send_text("never mind").await.expect("send");
    reply(&harness, "ok");
    assert_eq!(reap_idle(&harness).await, 1);
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(
        replied(&outputs, "ok"),
        "the queued message ran: {outputs:#?}"
    );
    let after = row(&harness, &ask_id).await;
    assert_eq!(after.resolution, Some(AskResolution::Abandoned));
    assert!(after.applied_at.is_some());
    assert!(adjacent_result(&harness).await.contains("withdrawn"));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(harness.strict_rejections.load(Ordering::SeqCst), 0);
    tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
        .await
        .expect("the actor quiesces");
}

/// Shutdown is not a release the actor carries on from: a message queued
/// behind the parked turn must not run, because its prelude would withdraw
/// the ask the next process is meant to re-show.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_queued_behind_a_parked_turn_does_not_run_at_shutdown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join(DB_FILE))
        .await
        .expect("open the database");
    let session = owner_session(TriggerSource::User);
    store
        .session
        .save(&session)
        .await
        .expect("save the session");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut harness = builder(&session, store, &runs).build();
    call_gated_write(&harness);
    harness.send_text("write it").await.expect("send");
    let ask_id = parked_ask(&harness).await;

    harness.send_text("never mind").await.expect("send");
    reply(&harness, "ok");
    harness.actor_parent_token.cancel();
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert!(!replied(&outputs, "ok"), "{outputs:#?}");
    let after = row(&harness, &ask_id).await;
    assert!(after.is_pending(), "left for the next process: {after:?}");
    assert!(after.applied_at.is_none());
    assert_eq!(completed_for_call(&outputs), 0, "nothing is appended");
}

const SLOW_TOOL: &str = "slow_sibling";

/// Runs until it is told to finish, so a test can hold it mid-execution.
struct SlowSibling {
    started: tokio::sync::mpsc::Sender<tokio_util::sync::CancellationToken>,
    finish: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Tool for SlowSibling {
    fn name(&self) -> &str {
        SLOW_TOOL
    }
    fn description(&self) -> String {
        "takes a while".to_string()
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    async fn execute(&self, _params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.started
            .try_send(ctx.cancellation_token.clone())
            .expect("report sibling start");
        self.finish.notified().await;
        Ok(ToolOutput::Text("done".to_string()))
    }
}

/// One call of the batch waits on an approval while its sibling is still
/// executing. The reaper's release would cut the sibling mid-run, so it
/// waits until the parked call is all that is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_turn_is_not_released_while_a_sibling_call_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join(DB_FILE))
        .await
        .expect("open the database");
    let session = owner_session(TriggerSource::User);
    store
        .session
        .save(&session)
        .await
        .expect("save the session");
    let runs = Arc::new(AtomicUsize::new(0));
    let (started, mut running) = tokio::sync::mpsc::channel(1);
    let finish = Arc::new(tokio::sync::Notify::new());
    let mut harness = builder(&session, store, &runs)
        .with_tool(
            Arc::new(SlowSibling {
                started,
                finish: Arc::clone(&finish),
            }),
            ToolManifest {
                name: SLOW_TOOL.to_string(),
                description: "takes a while".to_string(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: Vec::new(),
                channels: Vec::new(),
            },
        )
        .build();
    harness.stub_llm.push_stream(vec![
        StreamEvent::ToolCall(ToolCallInfo {
            id: CALL_ID.to_string(),
            name: GATED_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        }),
        StreamEvent::ToolCall(ToolCallInfo {
            id: "call-slow".to_string(),
            name: SLOW_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        }),
    ]);
    harness.send_text("write it").await.expect("send");
    let ask_id = parked_ask(&harness).await;
    let sibling_token = tokio::time::timeout(WAIT_TIMEOUT, running.recv())
        .await
        .expect("the sibling starts")
        .expect("sibling token");

    assert_eq!(reap_idle(&harness).await, 1);
    assert!(
        !sibling_token.is_cancelled(),
        "reaping must let the sibling finish"
    );
    assert_eq!(queue_liveness(&harness, &session, &ask_id), Some(true));

    finish.notify_one();
    tokio::time::timeout(WAIT_TIMEOUT, async {
        while !harness.supervisor.is_empty() {
            reap_idle(&harness).await;
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the reaper releases the turn once its sibling finishes");
    assert!(
        transcript(&harness)
            .await
            .iter()
            .flat_map(|row| &row.content)
            .any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "call-slow" && content.to_string().contains("done"))
            }),
        "the reaper let the sibling finish"
    );
    assert_eq!(queue_liveness(&harness, &session, &ask_id), Some(false));
    assert!(row(&harness, &ask_id).await.is_pending());
}

async fn reap_idle(harness: &AgentTestHarness) -> usize {
    harness
        .supervisor
        .reap_idle(&harness.session_manager, chrono::Duration::zero())
        .await
}

fn queue_liveness(harness: &AgentTestHarness, session: &Session, ask_id: &AskId) -> Option<bool> {
    harness
        .ask_queues
        .get_or_create(&session.channel)
        .liveness(ask_id)
}

/// Wait until a tool future in `harness`'s session is parked on an ask, and
/// return that ask's id.
async fn parked_ask(harness: &AgentTestHarness) -> AskId {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        while !harness.human_asks.has_live_pending_ask(&harness.session.id) {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the call parks on an approval");
    let rows = harness
        .ask_ledger
        .list_unapplied_for_session(&harness.session.id)
        .await
        .expect("list asks");
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows[0].ask_id.clone()
}

fn call_gated_write(harness: &AgentTestHarness) {
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
            id: CALL_ID.to_string(),
            name: GATED_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        })]);
}

/// A provider may reuse a `tool_use` id within one session. The repeat is
/// renamed before anything records it, so after a restart the answer finds
/// the call it was asked for — not the earlier call that already has a
/// result, which would leave the answer applied to nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_to_a_repeated_tool_use_id_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));

    let renamed = {
        let store = Store::open(&db).await.expect("open the database");
        store
            .session
            .save(&session)
            .await
            .expect("save the session");
        let mut harness = builder(&session, store, &runs).build();
        call_gated_write(&harness);
        harness.send_text("write it").await.expect("send");
        let first = parked_ask(&harness).await;
        reply(&harness, "first done");
        harness
            .human_asks
            .resolve(&first, AskResolution::Approval(ApprovalDecision::Approve))
            .await
            .expect("answer live");
        harness.drain_outputs(DRAIN_TIMEOUT).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        call_gated_write(&harness);
        harness.send_text("again").await.expect("send");
        let second = parked_ask(&harness).await;
        let renamed = row(&harness, &second).await.tool_call_id;
        assert_ne!(
            renamed, CALL_ID,
            "the repeat is renamed before it is recorded"
        );
        harness.actor_parent_token.cancel();
        harness.drain_outputs(DRAIN_TIMEOUT).await;
        tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
            .await
            .expect("the actor quiesces");
        (second, renamed)
    };
    let (second, renamed) = renamed;

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    reply(&node.harness, "second done");
    node.harness
        .human_asks
        .resolve(&second, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer the dead ask");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 2, "the parked call ran");
    let rows = transcript(&node.harness).await;
    let at = rows
        .iter()
        .position(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if *id == renamed))
        })
        .expect("the renamed tool_use is persisted under its new id");
    let result = rows[at + 1]
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if *tool_use_id == renamed => Some(content.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("its result is adjacent: {rows:#?}"));
    assert!(result.contains(WRITTEN), "{result}");
    assert!(replied(&outputs, "second done"), "{outputs:#?}");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
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
async fn a_question_survives_a_restart_and_its_answer_resumes_the_turn() {
    use baybo_model::human_ask::{AskKind, QuestionAnswer};
    use baybo_turn::{CancelReason, TurnStatus};

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let question_args = json!({
        "questions": [{
            "header": "Auth",
            "question": "Which auth strategy should we use?",
            "options": [
                { "label": "JWT", "description": "Stateless bearer tokens" },
                { "label": "Cookie", "description": "Server-side sessions" }
            ],
            "multi_select": false
        }]
    });

    // First boot: model invokes AskUser, parks on the question, then process shuts down.
    let (ask_id, settled_turn) = {
        let store = Store::open(&db).await.expect("open the database");
        store
            .session
            .save(&session)
            .await
            .expect("save the session");
        let mut harness = builder(&session, store, &runs).build();
        harness
            .stub_llm
            .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
                id: CALL_ID.to_string(),
                name: baybo_model::human_ask::ASK_USER_TOOL_NAME.to_string(),
                arguments: question_args.clone(),
                signature: None,
            })]);
        harness.send_text("set up auth").await.expect("send");
        let queue = harness.ask_queues.get_or_create(&session.channel);
        tokio::time::timeout(WAIT_TIMEOUT, async {
            while queue.is_empty() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("AskUser parks on a question");
        let rows = harness
            .ask_ledger
            .list_unapplied_for_session(&session.id)
            .await
            .expect("list asks");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, AskKind::Question);
        let turn_id = rows[0].turn_id;
        let turns = Arc::clone(&harness.turn_lifecycle);
        assert_eq!(harness.strict_rejections.load(Ordering::SeqCst), 0);
        tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
            .await
            .expect("the actor quiesces");
        let old_turn = turns.get(&turn_id).await.unwrap().unwrap();
        assert!(
            matches!(
                old_turn.status,
                TurnStatus::Cancelled {
                    reason: CancelReason::ParentCancelled,
                    ..
                }
            ),
            "shutdown returned with {:?}",
            old_turn.status
        );
        (rows[0].ask_id.clone(), old_turn)
    };

    // Second boot over the same sqlite db.
    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    let old_turn = node
        .harness
        .turn_lifecycle
        .get(&settled_turn.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_turn.status, settled_turn.status);

    let cards = node.owner.pending_asks(&session.id);
    assert_eq!(
        cards.len(),
        1,
        "SubscribeState carries the pending question"
    );
    assert_eq!(cards[0].ask_id, ask_id);
    assert_eq!(cards[0].tool_call_id, CALL_ID);
    let questions = &cards[0].question().unwrap().questions;
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].header, "Auth");
    assert!(
        node.owner.pending_approval_sessions().contains(&session.id),
        "the chat list reads awaiting_human for pending questions too"
    );
    tokio::time::timeout(WAIT_TIMEOUT, node.pushes.recv())
        .await
        .expect("the unsubscribed session receives one push")
        .expect("push stream open");
    no_more_pushes(&mut node.pushes).await;

    let rows = transcript(&node.harness).await;
    assert!(tool_use_at(&rows).is_some(), "{rows:#?}");
    assert!(
        rows.iter().all(|m| result_text(m).is_none()),
        "the AskUser tool_use stays dangling: {rows:#?}"
    );
    assert!(row(&node.harness, &ask_id).await.is_pending());

    reply(&node.harness, "configured JWT auth");
    let answers = vec![QuestionAnswer {
        header: "Auth".into(),
        selected: vec!["JWT".into()],
        other: None,
    }];
    node.harness
        .human_asks
        .resolve(&ask_id, AskResolution::Answered(answers.clone()))
        .await
        .expect("answer the dead question");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    let result = adjacent_result(&node.harness).await;
    assert!(
        result.contains("JWT"),
        "real tool_result persisted: {result}"
    );
    assert_eq!(resume_turns(&node.harness).await, 1);
    assert!(replied(&outputs, "configured JWT auth"), "{outputs:#?}");
    assert!(
        outputs.iter().any(|o| matches!(
            &o.event,
            AgentEvent::ToolCompleted { call_id, ask: Some(summary), .. }
                if call_id == CALL_ID
                    && !summary.declined
                    && summary.questions.len() == 1
                    && summary.questions[0].selected == vec!["JWT".to_string()]
        )),
        "Resume turn must emit ToolCompleted with typed AskStepSummary: {outputs:#?}"
    );
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert_eq!(node.harness.stub_llm.captured_requests().len(), 1);
    assert!(row(&node.harness, &ask_id).await.applied_at.is_some());
    assert!(node.owner.pending_asks(&session.id).is_empty());
    assert!(
        node.closed.lock().iter().any(|e| matches!(
            e,
            AskQueueEvent::AskClosed { ask_id: closed, outcome: AskResolution::Answered(_), .. } if *closed == ask_id
        )),
        "AskResolved goes out for the answered question"
    );
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
    assert!(adjacent_result(&node.harness).await.contains("withdrawn"));
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

/// A turn parked on two approvals when the process stopped. Answering the
/// first after the restart must not carry the turn on — its settlement would
/// abandon the second while its card is still up. The turn resumes once, on
/// the last answer, with both results in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_parked_on_two_asks_resumes_on_the_last_answer() {
    const SECOND_CALL: &str = "call-restart-2";
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));

    let (first, second) = {
        let store = Store::open(&db).await.expect("open the database");
        store
            .session
            .save(&session)
            .await
            .expect("save the session");
        let mut harness = builder(&session, store, &runs).build();
        harness.stub_llm.push_stream(
            [CALL_ID, SECOND_CALL]
                .into_iter()
                .map(|id| {
                    StreamEvent::ToolCall(ToolCallInfo {
                        id: id.to_string(),
                        name: GATED_TOOL.to_string(),
                        arguments: json!({}),
                        signature: None,
                    })
                })
                .collect(),
        );
        harness.send_text("write both").await.expect("send");
        let queue = harness.ask_queues.get_or_create(&session.channel);
        tokio::time::timeout(WAIT_TIMEOUT, async {
            while queue.len() < 2 {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("both calls park on an approval");
        harness.actor_parent_token.cancel();
        harness.drain_outputs(DRAIN_TIMEOUT).await;
        tokio::time::timeout(WAIT_TIMEOUT, harness.shutdown())
            .await
            .expect("the actor quiesces");
        let rows = {
            let store = Store::open(&db).await.expect("reopen the database");
            store
                .human_ask
                .list_unapplied_for_session(&session.id)
                .await
                .expect("list asks")
        };
        let id_of = |call: &str| {
            rows.iter()
                .find(|r| r.tool_call_id == call)
                .unwrap_or_else(|| panic!("an ask for {call}: {rows:?}"))
                .ask_id
                .clone()
        };
        (id_of(CALL_ID), id_of(SECOND_CALL))
    };

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    node.harness
        .human_asks
        .resolve(&first, AskResolution::Approval(ApprovalDecision::Approve))
        .await
        .expect("answer the first");
    node.harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(resume_turns(&node.harness).await, 0, "the turn waits");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(row(&node.harness, &second).await.is_pending());
    assert_eq!(
        node.owner
            .pending_approvals(&session.id)
            .iter()
            .map(|card| card.ask_id.clone())
            .collect::<Vec<_>>(),
        vec![second.clone()],
        "the second card stays up"
    );

    reply(&node.harness, "both done");
    node.harness
        .human_asks
        .resolve(&second, AskResolution::Approval(ApprovalDecision::Deny))
        .await
        .expect("answer the second");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(resume_turns(&node.harness).await, 1, "resumed once");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "only the approved call ran");
    assert!(adjacent_result(&node.harness).await.contains(WRITTEN));
    for ask_id in [&first, &second] {
        assert!(row(&node.harness, ask_id).await.applied_at.is_some());
    }
    assert!(replied(&outputs, "both done"), "{outputs:#?}");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
    assert_eq!(node.harness.stub_llm.captured_requests().len(), 1);
    stop(node).await;
}

/// A `/stop` that dropped the gate mid-insert: the loop found no ask and
/// appended its cancel fill, then the write-ahead row committed pending. The
/// next settlement must not answer the call a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settlement_never_answers_a_call_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(DB_FILE);
    let session = owner_session(TriggerSource::User);
    let runs = Arc::new(AtomicUsize::new(0));
    let ask_id = raise_then_shut_down(&db, &session, &runs).await;
    {
        let store = Store::open(&db).await.expect("open the database");
        store
            .session
            .append_session_message(
                &session.id,
                &ChatMessage::tool_result_with_meta(
                    CALL_ID.to_string(),
                    "cancelled".to_string(),
                    None,
                ),
            )
            .await
            .expect("append the cancel fill");
    }

    let mut node = boot(&db, &session, &runs, true, |_| {}).await;
    reply(&node.harness, "ok");
    node.harness.send_text("never mind").await.expect("send");
    let outputs = node.harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = row(&node.harness, &ask_id).await;
    assert_eq!(after.resolution, Some(AskResolution::Abandoned));
    assert!(after.applied_at.is_some());
    assert_eq!(adjacent_result(&node.harness).await, "cancelled");
    assert!(replied(&outputs, "ok"), "{outputs:#?}");
    assert_eq!(node.harness.strict_rejections.load(Ordering::SeqCst), 0);
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
    assert!(adjacent_result(&node.harness).await.contains("withdrawn"));

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
