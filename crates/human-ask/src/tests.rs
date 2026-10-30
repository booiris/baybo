//! Gate and manager behaviour over [`MemoryHumanAskStore`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::{
    AskId, AskKind, AskPhase, AskRequest, AskResolution, QuestionRequest, ResumeRequest,
};
use baybo_model::{ChannelType, SessionId};
use baybo_store::HumanAskRow;
use baybo_tools::{ApprovalGate, ApprovalOutcome, AskCancel, AskSite, TurnTokens};
use chrono::Utc;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::{AskCore, AskEvent};
use crate::error::HumanAskError;
use crate::gate::{ChannelAskGate, PolicyDenyGate};
use crate::manager::{HumanAskManager, HumanAskManagerConfig, ReloadScope, Settled};
use crate::ports::{AskObserver, ResumeSink};
use crate::queue::{AskQueue, AskQueueEvent, AskQueues, PendingEdge};
use crate::test_support::MemoryHumanAskStore;

const ISSUE_SESSION: &str = "issue-session";

#[derive(Default)]
struct RecordingSink {
    requests: Mutex<Vec<ResumeRequest>>,
}

#[async_trait]
impl ResumeSink for RecordingSink {
    async fn resumes_on_answer(&self, session_id: &SessionId) -> bool {
        session_id.as_str() != ISSUE_SESSION
    }

    fn request_resume(&self, req: ResumeRequest) {
        self.requests.lock().push(req);
    }
}

/// Records every observer event, and what the queue and ledger looked like
/// when it arrived.
struct RecordingObserver {
    queue: AskQueue,
    store: Arc<MemoryHumanAskStore>,
    seen: Mutex<Vec<(AskId, AskEvent, usize, bool)>>,
}

#[async_trait]
impl AskObserver for RecordingObserver {
    async fn on_event(&self, ask: &HumanAskRow, event: AskEvent) {
        let resolved_in_store = self
            .store
            .row(&ask.ask_id)
            .is_some_and(|r| r.resolved_at.is_some());
        self.seen.lock().push((
            ask.ask_id.clone(),
            event,
            self.queue.len(),
            resolved_in_store,
        ));
    }
}

struct Harness {
    store: Arc<MemoryHumanAskStore>,
    core: Arc<AskCore>,
    queues: AskQueues,
    queue: AskQueue,
    gate: Arc<ChannelAskGate>,
    manager: Arc<HumanAskManager>,
    sink: Arc<RecordingSink>,
    observer: Arc<RecordingObserver>,
    edges: Arc<Mutex<Vec<(SessionId, PendingEdge)>>>,
    woken: Arc<Mutex<Vec<AskId>>>,
}

fn harness() -> Harness {
    let store = MemoryHumanAskStore::new();
    let core = Arc::new(AskCore::new(store.clone()));
    let queues = AskQueues::new();
    let queue = queues.get_or_create(&ChannelType::owner());
    let edges = Arc::new(Mutex::new(Vec::new()));
    let edge_sink = Arc::clone(&edges);
    queue.add_pending_watcher(Arc::new(move |sid, edge| {
        edge_sink.lock().push((sid, edge))
    }));
    let woken = Arc::new(Mutex::new(Vec::new()));
    let woken_sink = Arc::clone(&woken);
    queue.add_ask_watcher(Arc::new(move |event| {
        if let AskQueueEvent::AskRaised(ask) = event {
            woken_sink.lock().push(ask.ask_id);
        }
    }));
    let gate = Arc::new(ChannelAskGate::new(queue.clone(), Arc::clone(&core)));
    let sink = Arc::new(RecordingSink::default());
    let manager = Arc::new(HumanAskManager::from_config(HumanAskManagerConfig {
        core: Arc::clone(&core),
        queues: queues.clone(),
        resume_sink: sink.clone(),
    }));
    let observer = Arc::new(RecordingObserver {
        queue: queue.clone(),
        store: store.clone(),
        seen: Mutex::new(Vec::new()),
    });
    core.add_observer(observer.clone());
    Harness {
        store,
        core,
        queues,
        queue,
        gate,
        manager,
        sink,
        observer,
        edges,
        woken,
    }
}

fn id(s: &str) -> AskId {
    s.parse().expect("ask id")
}

fn req(call_id: &str, session_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        call_id: call_id.into(),
        tool_call_id: format!("use-{call_id}"),
        session_id: session_id.into(),
        user_id: "u".into(),
        tool: "Bash".into(),
        accesses: vec![],
        params_preview: "{}".into(),
        description: None,
    }
}

fn site(phase: AskPhase) -> AskSite {
    AskSite {
        channel: ChannelType::owner(),
        phase,
    }
}

fn tokens() -> TurnTokens {
    TurnTokens {
        turn: CancellationToken::new(),
        actor: CancellationToken::new(),
    }
}

fn row(call_id: &str, session_id: &str, phase: AskPhase) -> HumanAskRow {
    HumanAskRow {
        ask_id: id(call_id),
        session_id: session_id.into(),
        channel: ChannelType::owner(),
        tool_call_id: format!("use-{call_id}"),
        kind: AskKind::Approval,
        phase,
        request: AskRequest::Approval(req(call_id, session_id)),
        created_at: Utc::now(),
        resolved_at: None,
        resolution: None,
        applied_at: None,
    }
}

fn question_row(call_id: &str, session_id: &str) -> HumanAskRow {
    HumanAskRow {
        kind: AskKind::Question,
        phase: AskPhase::DuringExecute,
        request: AskRequest::Question(QuestionRequest { questions: vec![] }),
        ..row(call_id, session_id, AskPhase::DuringExecute)
    }
}

fn resolved(mut row: HumanAskRow, resolution: AskResolution) -> HumanAskRow {
    row.resolved_at = Some(Utc::now());
    row.resolution = Some(resolution);
    row
}

/// Spawn a gate request and wait until it is parked on the queue.
async fn park(
    h: &Harness,
    call_id: &str,
    session_id: &str,
    phase: AskPhase,
    tokens: TurnTokens,
) -> tokio::task::JoinHandle<ApprovalOutcome> {
    let gate = Arc::clone(&h.gate);
    let request = req(call_id, session_id);
    let task = tokio::spawn(async move {
        gate.request(request, site(phase), AskCancel::new(tokens))
            .await
    });
    let ask_id = id(call_id);
    for _ in 0..1000 {
        if h.queue.contains(&ask_id) {
            return task;
        }
        tokio::task::yield_now().await;
    }
    panic!("gate never parked {call_id}");
}

const APPROVE: AskResolution = AskResolution::Approval(ApprovalDecision::Approve);

#[tokio::test]
async fn write_ahead_row_and_observer_precede_the_queue_push() {
    let h = harness();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, tokens()).await;
    let stored = h.store.row(&id("a")).expect("row written");
    assert!(stored.is_pending());
    assert_eq!(stored.channel, ChannelType::owner());
    assert_eq!(stored.phase, AskPhase::BeforeExecute);
    assert_eq!(
        h.observer.seen.lock()[0],
        (id("a"), AskEvent::Raised, 0, false),
        "observer ran after the insert and before the push"
    );
    assert_eq!(*h.woken.lock(), vec![id("a")]);
    task.abort();
}

#[tokio::test]
async fn a_failing_create_raises_no_entry_and_no_edge() {
    let h = harness();
    h.store.fail_creates();
    let out = h
        .gate
        .request(
            req("a", "s1"),
            site(AskPhase::BeforeExecute),
            AskCancel::never(),
        )
        .await;
    assert_eq!(out, ApprovalOutcome::abandoned());
    assert!(h.queue.is_empty());
    assert!(h.edges.lock().is_empty());
    assert!(h.woken.lock().is_empty());
}

#[tokio::test]
async fn live_resolve_persists_first_then_fires_and_the_gate_stamps_applied() {
    let h = harness();
    let task = park(&h, "a", "s1", AskPhase::DuringExecute, tokens()).await;
    h.manager.resolve(&id("a"), APPROVE).await.expect("resolve");
    assert_eq!(
        task.await.expect("join"),
        ApprovalOutcome::answered(ApprovalDecision::Approve)
    );
    let seen = h.observer.seen.lock().clone();
    assert_eq!(
        seen[1],
        (id("a"), AskEvent::Resolved(APPROVE), 1, true),
        "the claim landed while the entry was still queued"
    );
    let stored = h.store.row(&id("a")).expect("row");
    assert_eq!(stored.resolution, Some(APPROVE));
    assert!(stored.applied_at.is_some(), "the gate stamps applied");
    assert!(h.sink.requests.lock().is_empty(), "live: no resume");
    assert_eq!(
        h.edges.lock().last().map(|(_, e)| e.clone()),
        Some(PendingEdge::Answered)
    );
}

#[tokio::test]
async fn a_second_resolve_is_ask_closed() {
    let h = harness();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, tokens()).await;
    h.manager.resolve(&id("a"), APPROVE).await.expect("first");
    let second = h
        .manager
        .resolve(&id("a"), AskResolution::Approval(ApprovalDecision::Deny))
        .await;
    assert!(matches!(second, Err(HumanAskError::AskClosed { .. })));
    task.await.expect("join");
}

#[tokio::test]
async fn a_question_resolution_does_not_fit_an_approval() {
    let h = harness();
    h.store.insert(row("a", "s1", AskPhase::BeforeExecute));
    let out = h.manager.resolve(&id("a"), AskResolution::Declined).await;
    assert!(matches!(out, Err(HumanAskError::KindMismatch { .. })));
    assert!(h.store.row(&id("a")).expect("row").is_pending());
}

#[tokio::test]
async fn resolving_a_dead_ask_removes_it_and_requests_a_resume() {
    let h = harness();
    h.store.insert(row("a", "s1", AskPhase::BeforeExecute));
    h.manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    assert_eq!(h.queue.liveness(&id("a")), Some(false));
    h.edges.lock().clear();

    h.manager.resolve(&id("a"), APPROVE).await.expect("resolve");
    assert!(h.queue.is_empty());
    assert!(h.queue.pending_sessions().is_empty());
    assert_eq!(
        *h.edges.lock(),
        vec![(SessionId::from("s1"), PendingEdge::Answered)]
    );
    assert_eq!(
        *h.sink.requests.lock(),
        vec![ResumeRequest {
            session_id: "s1".into(),
            ask_id: id("a"),
        }]
    );
    assert_eq!(
        h.manager
            .unapplied_tool_use_ids(&"s1".into())
            .await
            .expect("ids"),
        vec!["use-a".to_string()],
        "store-keyed: the answered ask is still unapplied"
    );
}

#[tokio::test]
async fn resolving_a_dead_ask_in_an_issue_session_requests_nothing() {
    let h = harness();
    h.store
        .insert(row("a", ISSUE_SESSION, AskPhase::BeforeExecute));
    h.manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    h.manager.resolve(&id("a"), APPROVE).await.expect("resolve");
    assert!(h.sink.requests.lock().is_empty());
    assert_eq!(
        h.store.row(&id("a")).expect("row").resolution,
        Some(APPROVE)
    );
}

#[tokio::test]
async fn turn_cancel_returns_abandoned_and_leaves_the_row_pending() {
    let h = harness();
    let t = tokens();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, t.clone()).await;
    t.turn.cancel();
    assert_eq!(task.await.expect("join"), ApprovalOutcome::abandoned());
    let stored = h.store.row(&id("a")).expect("row");
    assert!(stored.is_pending(), "the gate never writes on cancel");
    assert!(stored.applied_at.is_none());
    assert_eq!(h.queue.liveness(&id("a")), Some(false), "orphaned, dead");

    // The loop's claim, made before it appends the denial.
    assert!(
        h.manager
            .abandon_for_tool_call(&"s1".into(), "use-a")
            .await
            .expect("claim"),
        "the call had an ask"
    );
    let stored = h.store.row(&id("a")).expect("row");
    assert_eq!(stored.resolution, Some(AskResolution::Abandoned));
    assert!(stored.applied_at.is_some());
    assert!(h.queue.is_empty());
    assert_eq!(
        h.edges.lock().last().map(|(_, e)| e.clone()),
        Some(PendingEdge::Abandoned),
        "abandon clears the session mark"
    );
}

#[tokio::test]
async fn abandon_for_tool_call_consumes_an_answer_that_won_the_race() {
    let h = harness();
    h.store
        .insert(resolved(row("a", "s1", AskPhase::BeforeExecute), APPROVE));
    h.store
        .insert(resolved(row("b", "s1", AskPhase::DuringExecute), APPROVE));
    for call in ["use-a", "use-b"] {
        assert!(
            h.manager
                .abandon_for_tool_call(&"s1".into(), call)
                .await
                .expect("claim"),
            "the loop appends the cancel fill for {call}"
        );
    }
    for ask in ["a", "b"] {
        let stored = h.store.row(&id(ask)).expect("row");
        assert_eq!(stored.resolution, Some(APPROVE), "the answer stands");
        assert!(stored.applied_at.is_some(), "consumed by the fill");
    }
    assert!(
        h.manager
            .settle_unapplied(&"s1".into())
            .await
            .expect("settle")
            .is_empty(),
        "nothing left for a second result"
    );
    assert!(
        !h.manager
            .abandon_for_tool_call(&"s1".into(), "use-other")
            .await
            .expect("claim")
    );
}

#[tokio::test]
async fn a_mid_run_answer_racing_stop_still_gets_one_result() {
    let h = harness();
    let t = tokens();
    let task = park(&h, "a", "s1", AskPhase::DuringExecute, t.clone()).await;
    t.turn.cancel();
    assert_eq!(task.await.expect("join"), ApprovalOutcome::abandoned());
    assert!(
        h.queue.is_empty(),
        "a non-resumable orphan leaves the queue"
    );
    // The answer lands between the gate dropping and the loop's claim.
    h.manager.resolve(&id("a"), APPROVE).await.expect("resolve");
    assert!(h.sink.requests.lock().is_empty());
    assert!(
        h.manager
            .abandon_for_tool_call(&"s1".into(), "use-a")
            .await
            .expect("claim"),
        "the loop appends the cancel fill"
    );
    assert!(h.store.row(&id("a")).expect("row").applied_at.is_some());
}

#[tokio::test]
async fn ancestor_cancel_never_returns() {
    let h = harness();
    let t = tokens();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, t.clone()).await;
    t.actor.cancel();
    t.turn.cancel();
    let out = tokio::time::timeout(Duration::from_millis(50), task).await;
    assert!(out.is_err(), "stays parked on an ancestor cancel");
    assert!(h.store.row(&id("a")).expect("row").is_pending());
}

#[tokio::test]
async fn a_live_waiter_sees_abandon() {
    let h = harness();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, tokens()).await;
    assert!(h.manager.abandon(&id("a")).await.expect("abandon"));
    assert_eq!(task.await.expect("join"), ApprovalOutcome::abandoned());
    let stored = h.store.row(&id("a")).expect("row");
    assert_eq!(stored.resolution, Some(AskResolution::Abandoned));
    assert!(stored.applied_at.is_some(), "the waiter consumed it");
    assert!(!h.manager.abandon(&id("a")).await.expect("second"));
}

#[tokio::test]
async fn abandon_for_session_takes_live_and_dead_asks() {
    let h = harness();
    h.store.insert(row("dead", "s1", AskPhase::BeforeExecute));
    h.manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    let live = park(&h, "live", "s1", AskPhase::BeforeExecute, tokens()).await;
    assert!(h.manager.has_live_pending_ask(&"s1".into()));
    assert_eq!(
        h.manager
            .abandon_for_session(&"s1".into())
            .await
            .expect("abandon"),
        2
    );
    assert_eq!(live.await.expect("join"), ApprovalOutcome::abandoned());
    assert!(h.queue.is_empty());
    assert!(!h.manager.has_live_pending_ask(&"s1".into()));
}

#[tokio::test]
async fn policy_deny_writes_an_applied_row_and_no_entry() {
    let h = harness();
    let gate = PolicyDenyGate::new(Arc::clone(&h.core));
    let out = gate
        .request(
            req("a", "s1"),
            site(AskPhase::DuringExecute),
            AskCancel::never(),
        )
        .await;
    assert_eq!(out, ApprovalOutcome::policy(ApprovalDecision::Deny));
    let stored = h.store.row(&id("a")).expect("row");
    assert_eq!(stored.resolution, Some(AskResolution::PolicyDenied));
    assert!(stored.applied_at.is_some());
    assert!(h.queue.is_empty());
    assert!(h.edges.lock().is_empty());
    assert_eq!(
        h.observer.seen.lock().clone(),
        vec![(
            id("a"),
            AskEvent::Resolved(AskResolution::PolicyDenied),
            0,
            true
        )]
    );
}

#[tokio::test]
async fn reload_requeues_pending_resumable_rows_per_channel() {
    let h = harness();
    h.store.insert(row("a", "s1", AskPhase::BeforeExecute));
    h.store.insert(row("b", "s1", AskPhase::BeforeExecute));
    h.store.insert(question_row("q", "s2"));
    let mut sidecar = row("x", "s3", AskPhase::BeforeExecute);
    sidecar.channel = ChannelType::from("custom");
    h.store.insert(sidecar);

    let report = h
        .manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    assert_eq!(report.requeued, 4);
    assert_eq!(h.queue.len(), 3);
    let raised: Vec<SessionId> = h
        .edges
        .lock()
        .iter()
        .filter(|(_, e)| e.is_pending())
        .map(|(s, _)| s.clone())
        .collect();
    assert_eq!(
        raised,
        vec![SessionId::from("s1"), SessionId::from("s2")],
        "Raised once per session"
    );
    let custom = h
        .queues
        .get(&ChannelType::from("custom"))
        .expect("created for a channel not installed yet");
    assert!(custom.contains(&id("x")));
    assert!(h.observer.seen.lock().is_empty(), "reload notifies nobody");

    let again = h
        .manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload twice");
    assert_eq!(again.requeued, 0, "idempotent");
    assert_eq!(h.queue.len(), 3);
}

#[tokio::test]
async fn reload_resumes_resolved_rows_without_showing_them() {
    let h = harness();
    h.store
        .insert(resolved(row("a", "s1", AskPhase::BeforeExecute), APPROVE));
    h.store.insert(resolved(
        row("b", ISSUE_SESSION, AskPhase::BeforeExecute),
        APPROVE,
    ));
    let report = h
        .manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    assert_eq!(report.resumed, 1);
    assert!(h.queue.is_empty(), "not re-shown");
    assert!(h.edges.lock().is_empty(), "no Raised, so no push");
    assert_eq!(
        *h.sink.requests.lock(),
        vec![ResumeRequest {
            session_id: "s1".into(),
            ask_id: id("a"),
        }]
    );
}

#[tokio::test]
async fn a_one_shot_reload_leaves_answered_asks_alone() {
    let h = harness();
    h.store
        .insert(resolved(row("a", "s1", AskPhase::BeforeExecute), APPROVE));
    h.store.insert(row("p", "s1", AskPhase::BeforeExecute));
    let report = h
        .manager
        .reload_pending(ReloadScope::LeaveAnswered)
        .await
        .expect("reload");
    assert_eq!((report.resumed, report.requeued), (0, 1));
    assert!(h.sink.requests.lock().is_empty());
    assert!(h.store.row(&id("a")).expect("row").applied_at.is_none());
}

#[tokio::test]
async fn reload_reconciles_non_resumable_rows() {
    let h = harness();
    h.store.insert(row("p", "s1", AskPhase::DuringExecute));
    h.store
        .insert(resolved(row("r", "s1", AskPhase::DuringExecute), APPROVE));
    let report = h
        .manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    assert_eq!((report.abandoned, report.applied), (1, 1));
    let p = h.store.row(&id("p")).expect("row");
    assert_eq!(p.resolution, Some(AskResolution::Abandoned));
    assert!(p.applied_at.is_some());
    assert!(h.store.row(&id("r")).expect("row").applied_at.is_some());
    assert!(h.queue.is_empty());
    let events: Vec<(AskId, AskEvent)> = h
        .observer
        .seen
        .lock()
        .iter()
        .map(|(id, e, _, _)| (id.clone(), e.clone()))
        .collect();
    assert_eq!(
        events,
        vec![(id("p"), AskEvent::Resolved(AskResolution::Abandoned))],
        "the board hears the abandon; nothing is re-raised"
    );

    let again = h
        .manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("again");
    assert_eq!(again, Default::default(), "idempotent");
}

#[tokio::test]
async fn settle_unapplied_handles_every_row_state() {
    let h = harness();
    h.store.insert(row("dead", "s1", AskPhase::BeforeExecute));
    h.store.insert(resolved(
        row("answered", "s1", AskPhase::BeforeExecute),
        APPROVE,
    ));
    h.store.insert(resolved(
        row("midrun", "s1", AskPhase::DuringExecute),
        APPROVE,
    ));
    h.manager
        .reload_pending(ReloadScope::ResumeAnswered)
        .await
        .expect("reload");
    // `midrun` was stamped applied by the reload; seed a fresh one.
    h.store.insert(resolved(
        row("midrun2", "s1", AskPhase::DuringExecute),
        APPROVE,
    ));
    // Pending, non-resumable, its waiter gone (an unwound actor): hydration
    // already filled its call.
    h.store.insert(row("orphan", "s1", AskPhase::DuringExecute));
    h.sink.requests.lock().clear();

    let settled = h
        .manager
        .settle_unapplied(&"s1".into())
        .await
        .expect("settle");
    assert_eq!(
        settled,
        vec![
            Settled::Abandoned {
                tool_call_id: "use-dead".into(),
                kind: AskKind::Approval,
            },
            Settled::Apply {
                ask_id: id("answered"),
                tool_call_id: "use-answered".into(),
                kind: AskKind::Approval,
                resolution: APPROVE,
            },
        ]
    );
    assert!(h.store.row(&id("dead")).expect("row").applied_at.is_some());
    let orphan = h.store.row(&id("orphan")).expect("row");
    assert_eq!(orphan.resolution, Some(AskResolution::Abandoned));
    assert!(orphan.applied_at.is_some(), "reconciled, nothing appended");
    assert!(
        h.store
            .row(&id("midrun2"))
            .expect("row")
            .applied_at
            .is_some()
    );
    assert!(
        h.store
            .row(&id("answered"))
            .expect("row")
            .applied_at
            .is_none(),
        "the caller stamps Apply rows"
    );
    assert!(h.queue.is_empty());
}

#[tokio::test]
async fn settle_unapplied_leaves_a_live_ask_alone() {
    let h = harness();
    let task = park(&h, "live", "s1", AskPhase::BeforeExecute, tokens()).await;
    let settled = h
        .manager
        .settle_unapplied(&"s1".into())
        .await
        .expect("settle");
    assert!(settled.is_empty());
    assert!(h.store.row(&id("live")).expect("row").is_pending());
    assert_eq!(h.queue.liveness(&id("live")), Some(true));
    task.abort();
}

/// A store whose session listing is stale by one concurrent resolve — the
/// window between `settle_unapplied`'s list and its claim.
struct RacingStore {
    inner: Arc<MemoryHumanAskStore>,
    answer: Mutex<Option<(AskId, AskResolution)>>,
}

#[async_trait]
impl baybo_store::HumanAskStore for RacingStore {
    async fn create(&self, row: &HumanAskRow) -> baybo_store::human_ask::Result<()> {
        self.inner.create(row).await
    }
    async fn resolve(
        &self,
        ask_id: &AskId,
        resolution: &AskResolution,
        now: chrono::DateTime<Utc>,
    ) -> baybo_store::human_ask::Result<bool> {
        self.inner.resolve(ask_id, resolution, now).await
    }
    async fn mark_applied(
        &self,
        ask_id: &AskId,
        now: chrono::DateTime<Utc>,
    ) -> baybo_store::human_ask::Result<bool> {
        self.inner.mark_applied(ask_id, now).await
    }
    async fn get(&self, ask_id: &AskId) -> baybo_store::human_ask::Result<Option<HumanAskRow>> {
        self.inner.get(ask_id).await
    }
    async fn list_unapplied(&self) -> baybo_store::human_ask::Result<Vec<HumanAskRow>> {
        self.inner.list_unapplied().await
    }
    async fn list_unapplied_for_session(
        &self,
        session_id: &SessionId,
    ) -> baybo_store::human_ask::Result<Vec<HumanAskRow>> {
        let rows = self.inner.list_unapplied_for_session(session_id).await?;
        let answer = self.answer.lock().take();
        if let Some((ask_id, resolution)) = answer {
            self.inner.resolve(&ask_id, &resolution, Utc::now()).await?;
        }
        Ok(rows)
    }
}

#[tokio::test]
async fn settle_unapplied_returns_apply_when_a_resolve_wins_the_claim() {
    let inner = MemoryHumanAskStore::new();
    inner.insert(row("a", "s1", AskPhase::BeforeExecute));
    let store = Arc::new(RacingStore {
        inner: inner.clone(),
        answer: Mutex::new(Some((id("a"), APPROVE))),
    });
    let manager = HumanAskManager::from_config(HumanAskManagerConfig {
        core: Arc::new(AskCore::new(store)),
        queues: AskQueues::new(),
        resume_sink: Arc::new(RecordingSink::default()),
    });
    let settled = manager
        .settle_unapplied(&"s1".into())
        .await
        .expect("settle");
    assert_eq!(
        settled,
        vec![Settled::Apply {
            ask_id: id("a"),
            tool_call_id: "use-a".into(),
            kind: AskKind::Approval,
            resolution: APPROVE,
        }]
    );
}

#[tokio::test]
async fn observers_hear_every_verb() {
    let h = harness();
    let task = park(&h, "a", "s1", AskPhase::BeforeExecute, tokens()).await;
    h.manager.resolve(&id("a"), APPROVE).await.expect("resolve");
    task.await.expect("join");
    h.store.insert(row("b", "s1", AskPhase::BeforeExecute));
    h.manager.abandon(&id("b")).await.expect("abandon");
    let events: Vec<(AskId, AskEvent)> = h
        .observer
        .seen
        .lock()
        .iter()
        .map(|(id, e, _, _)| (id.clone(), e.clone()))
        .collect();
    assert_eq!(
        events,
        vec![
            (id("a"), AskEvent::Raised),
            (id("a"), AskEvent::Resolved(APPROVE)),
            (id("b"), AskEvent::Resolved(AskResolution::Abandoned)),
        ]
    );
}

#[tokio::test]
async fn each_raise_is_announced_with_its_own_ask() {
    let h = harness();
    let a = park(&h, "a", "s1", AskPhase::BeforeExecute, tokens()).await;
    let b = park(&h, "b", "s2", AskPhase::BeforeExecute, tokens()).await;
    let seen: HashSet<AskId> = h.woken.lock().iter().cloned().collect();
    assert_eq!(seen, HashSet::from([id("a"), id("b")]));
    assert_eq!(h.woken.lock().len(), 2, "each announced exactly once");
    a.abort();
    b.abort();
}
