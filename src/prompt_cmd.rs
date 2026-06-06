//! Headless one-shot prompt mode (`aura prompt`), the non-interactive
//! sibling of `aura tui`.
//!
//! Hybrid runtime selection keyed off the workspace singleton lock:
//!
//! * **A gateway is running** (it holds `<workspace>/state/aura.lock`):
//!   route the turn through it over the same WS+MessagePack transport the
//!   TUI uses. One owner of the session state, no contention.
//! * **No gateway** (the lock is free): acquire it and build the agent
//!   runtime in-process for this one turn via [`crate::runtime`], run the
//!   message to completion, then tear down. This is what lets `aura
//!   prompt` work standalone, without first starting `aura gateway`.
//!
//! Either way the answer streams to stdout, logs go to stderr, and a
//! `--json` run emits one structured object. Resume is a pinned session
//! id — the agent rehydrates that session's context (server-side in the
//! gateway case, from the durable row in the in-process case) and only
//! the new turn's output is printed, never the prior transcript.
//!
//! Tool approvals have no human here. The WS path receives the gateway's
//! approval prompt and auto-resolves it over the wire; the in-process
//! path installs a session-scoped auto gate. Both default to `Deny`
//! (safe, fail-closed) and switch to `Approve` under
//! `--dangerously-allow-all`.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use aura_agent::service::ShutdownSignal;
use aura_channels::{
    AgentEvent, Connection, ConnectionSink, IncomingMessage, Message, NoticeLevel, SendOutcome,
    SessionEvent,
};
use aura_config::AuraConfig;
use aura_model::{ChannelType, ContentBlock, MessageMetadata, SessionId, User};
use aura_tools::{ApprovalDecision, ApprovalGate, ApprovalQueue, ApprovalRequest, AutoDenyGate};
use aura_tui::{TransportEvent, TransportEventStream};
use chrono::Utc;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::gateway_client::{
    admin_addr_from_config, read_tui_token, try_connect_with_token, unreachable_gateway_error,
};
use crate::runtime::{build_leak_detector, build_managers, force_exit_watchdog, wire_router};
use crate::tracing_init::{TracingMode, init_tracing};

/// Capacity of the in-process sink → consumer hop. Same order as the
/// gateway's outbound buffer — deep enough to absorb a burst of answer
/// deltas between `recv`s.
const INPROCESS_EVENT_CAPACITY: usize = 256;

/// How long teardown gets before the watchdog force-exits the process.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Upper bound on flushing detached cost-record writes before teardown —
/// generous for a libsql write, short enough not to hang the CLI if one
/// wedges.
const COST_DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// Resolved options for one headless turn, populated by `main` from the
/// parsed top-level flags.
pub struct Options {
    /// The user prompt to send (already merged with any piped stdin and
    /// validated non-empty by the caller).
    pub prompt: String,
    /// Resume this session id instead of minting a fresh one.
    pub session: Option<String>,
    /// Auto-approve tool approvals instead of the default auto-deny.
    pub allow_all: bool,
    /// Emit one structured JSON object instead of streaming prose.
    pub json: bool,
    /// Bound on how long to wait for the turn's reply. `None` waits
    /// indefinitely; `Some(d)` errors out after `d` so a turn the runtime
    /// rejects before replying (budget / rate-limit / security — none of
    /// which dispatch anything to the client) can't hang the command.
    pub timeout: Option<Duration>,
}

/// Apply the optional turn timeout to a consume future. `None` waits
/// indefinitely; `Some(d)` returns an error if no terminal reply arrives
/// within `d`. This is the bound that keeps a pre-response failure from
/// blocking the consume loop forever — the router only *logs* a
/// rejected `handle_incoming` (rate-limit / budget / security), so the
/// client otherwise never sees a terminal event.
async fn await_turn(
    budget: Option<Duration>,
    consume: impl std::future::Future<Output = anyhow::Result<String>>,
) -> anyhow::Result<String> {
    match budget {
        None => consume.await,
        Some(d) => match tokio::time::timeout(d, consume).await {
            Ok(result) => result,
            Err(_elapsed) => Err(anyhow::anyhow!(
                "no reply within {}s — the turn may have been rejected before replying \
                 (rate limit, budget, or security) or stalled; raise the bound with \
                 `--timeout <secs>`, or `--timeout 0` to wait indefinitely",
                d.as_secs()
            )),
        },
    }
}

/// Run a single prompt and return once the turn completes. Picks the
/// gateway-backed or in-process path based on whether a gateway holds the
/// workspace singleton lock.
pub async fn run(config: Arc<AuraConfig>, opts: Options) -> anyhow::Result<()> {
    // stdout is reserved for the assistant's answer; logs + notices → stderr.
    let _guards = init_tracing(TracingMode::Stderr);

    // The caller pins the session id: an explicit `--session`, else a
    // fresh UUID we mint here. Resolve once so both paths agree and the
    // `--json` object reports the id the turn actually used. A run started
    // without `--session` exposes its id only via `--json`; plain-text
    // output stays just the answer.
    let session_id = resolve_session_id(opts.session.as_deref());
    // In `--json` every outcome is a structured object on stdout, so a
    // failed turn still emits a parseable `{session_id, error}` instead of
    // only a human string on stderr. Captured before `opts` is moved.
    let json = opts.json;

    // The workspace singleton lock doubles as the gateway-presence
    // detector: a running gateway already holds it, so acquiring succeeds
    // only when none is running. Acquire on the exact path the gateway
    // uses (`gateway_cmd::start`) so the flock actually collides.
    let workspace_paths =
        aura_workspace::WorkspacePaths::new(std::path::PathBuf::from(&config.workspace.path));
    let result = match crate::singleton::acquire(workspace_paths.root()) {
        Ok(lock) => {
            info!("no running gateway; serving the prompt in-process");
            run_in_process(config, opts, lock, session_id.clone()).await
        }
        Err(_) => {
            info!("a gateway holds the workspace lock; routing the prompt over WS");
            run_via_gateway(config, opts, session_id.clone()).await
        }
    };

    // JSON mode renders failures as the structured shape on stdout and
    // exits non-zero, rather than letting `main` print a human error to
    // stderr. Text mode propagates the error unchanged.
    if let Err(e) = &result
        && json
    {
        emit_json_error(&session_id, e);
        std::process::exit(1);
    }
    result
}

// --------------------------------------------------------------------------
// Gateway-backed path (a gateway is already running)
// --------------------------------------------------------------------------

async fn run_via_gateway(
    config: Arc<AuraConfig>,
    opts: Options,
    session_id: SessionId,
) -> anyhow::Result<()> {
    let admin_addr = admin_addr_from_config(&config)?;

    let tui_token = read_tui_token(&config).await;
    let transport = Arc::new(
        try_connect_with_token(admin_addr, tui_token.as_deref(), &session_id)
            .await
            .map_err(|e| unreachable_gateway_error(admin_addr, &e.to_string()))?,
    );
    info!(%admin_addr, %session_id, "connected to gateway for one-shot prompt");

    let decision = approval_decision(opts.allow_all);

    // Subscribe before submitting so the turn's first frames can't race
    // ahead of the pump.
    let mut events = transport.subscribe().await?;
    transport
        .submit(build_incoming(&session_id, &opts.prompt))
        .await?;

    let approvals = transport.approval_queue();
    let answer = await_turn(
        opts.timeout,
        consume_ws_turn(&approvals, &mut events, decision, opts.json),
    )
    .await?;

    if opts.json {
        emit_json(&session_id, &answer);
    }
    Ok(())
}

async fn consume_ws_turn(
    approvals: &ApprovalQueue,
    events: &mut TransportEventStream,
    decision: ApprovalDecision,
    json: bool,
) -> anyhow::Result<String> {
    let mut writer = AnswerWriter::new(json);
    while let Some(item) = events.next().await {
        match item.map_err(|e| anyhow::anyhow!("gateway stream error: {e}"))? {
            TransportEvent::StreamDelta(text) => writer.delta(&text),
            TransportEvent::Response(blocks) => {
                writer.finalize(&blocks_text(&blocks));
                return Ok(writer.into_answer());
            }
            TransportEvent::ApprovalRequested => {
                // No human to decide; resolve over the wire so the turn
                // never stalls on the gateway's approval-gate timeout.
                let tool = approvals.peek_head().map(|r| r.tool).unwrap_or_default();
                approvals.resolve_head(decision);
                note_approval(decision, &tool);
            }
            TransportEvent::Notice { level, text } => eprint_notice(level, &text),
            TransportEvent::ToolStarted { .. }
            | TransportEvent::ToolCompleted { .. }
            | TransportEvent::Status { .. }
            | TransportEvent::ApprovalResolved { .. } => {}
        }
    }
    writer.finish_incomplete()
}

// --------------------------------------------------------------------------
// In-process path (no gateway running; we own the runtime for one turn)
// --------------------------------------------------------------------------

async fn run_in_process(
    config: Arc<AuraConfig>,
    opts: Options,
    // Held for the whole turn so no gateway / another `aura prompt` can
    // race the workspace. Dropped on return, releasing the flock.
    _lock: crate::singleton::WorkspaceLock,
    session_id: SessionId,
) -> anyhow::Result<()> {
    let shutdown = ShutdownSignal::new();
    let leak_detector = build_leak_detector(&config.security, &[]);
    let mut graph = build_managers(
        Arc::clone(&config),
        // One-shot: no config hot-reload (nothing re-reads the file).
        None,
        shutdown.clone(),
        leak_detector,
        // No embedded MCP servers — the browser tool is gateway-only.
        Vec::new(),
    )
    .await?;

    // Override the channel's interactive approval gate with a session-
    // scoped auto gate. Session-level wins over the type-level gate
    // `install_channels` registered, so approvals resolve instantly here
    // instead of fanning out to a UI that doesn't exist.
    let gate: Arc<dyn ApprovalGate> = if opts.allow_all {
        Arc::new(AutoApproveGate)
    } else {
        Arc::new(AutoDenyGate)
    };
    graph.channels_registry.approval_gates().insert_session(
        ChannelType::tui(),
        session_id.clone(),
        gate,
    );

    // Attach an in-process sink + subscribe so the router's per-session
    // fan-out delivers this turn's output to our mpsc.
    let mut events = attach_inprocess_sink(&graph.channels_registry, &session_id)?;

    let handle = wire_router(&mut graph).await;
    let incoming_tx = handle.incoming_tx;
    tokio::spawn(handle.router.run(handle.incoming_rx, handle.response_rx));

    incoming_tx
        .send(build_incoming(&session_id, &opts.prompt))
        .await
        .map_err(|e| anyhow::anyhow!("failed to submit prompt to router: {e}"))?;

    let answer = await_turn(opts.timeout, consume_inprocess_turn(&mut events, opts.json)).await;

    // Flush detached cost-record writes before teardown. `record_call`
    // persists on a spawned task; the long-running gateway outlives
    // those, but here the process exits seconds later, so an un-drained
    // write would be aborted and the turn's spend lost. Bounded so a
    // wedged libsql write can't hang the CLI.
    let _ = tokio::time::timeout(COST_DRAIN_BUDGET, graph.cost_manager.drain()).await;

    // Tear the runtime down: trip the shutdown signal (cascades to actors
    // + background tasks) and arm the watchdog so a stuck blocking call
    // can't wedge the exit. The turn is already finished by here, so the
    // happy-path teardown completes well inside the budget.
    shutdown.trigger();
    force_exit_watchdog(TEARDOWN_BUDGET);
    drop(graph);

    let answer = answer?;
    if opts.json {
        emit_json(&session_id, &answer);
    }
    Ok(())
}

/// Register an in-process [`ConnectionSink`] on the CLI channel and
/// subscribe it to `session_id`, returning the receiver the turn's
/// [`SessionEvent`]s arrive on.
fn attach_inprocess_sink(
    channels: &aura_channels::ChannelRegistry,
    session_id: &SessionId,
) -> anyhow::Result<mpsc::Receiver<SessionEvent>> {
    let channel = channels.get(&ChannelType::tui()).ok_or_else(|| {
        anyhow::anyhow!("CLI channel not installed; set channels.cli.enabled = true in aura.json")
    })?;
    let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(INPROCESS_EVENT_CAPACITY);
    let conn = Arc::new(Connection::new(Arc::new(InProcessSink { event_tx })));
    let conn_id = conn.id();
    channel.attach(Arc::clone(&conn));
    channel
        .as_subscribed()
        .ok_or_else(|| anyhow::anyhow!("CLI channel is not subscribed-kind"))?
        .subscribe(conn_id, session_id.clone())
        .map_err(|e| anyhow::anyhow!("subscribe in-process sink: {e}"))?;
    Ok(event_rx)
}

async fn consume_inprocess_turn(
    events: &mut mpsc::Receiver<SessionEvent>,
    json: bool,
) -> anyhow::Result<String> {
    let mut writer = AnswerWriter::new(json);
    while let Some(event) = events.recv().await {
        // UserEcho / approval fan-out aren't part of the answer (and the
        // session-scoped auto gate means approval events don't even fire).
        let SessionEvent::Agent(out) = event else {
            continue;
        };
        match out.event {
            AgentEvent::AnswerDelta(text) => writer.delta(&text),
            AgentEvent::Message(outgoing) => {
                writer.finalize(&blocks_text(&outgoing.content));
                return Ok(writer.into_answer());
            }
            AgentEvent::Notice { level, text } => eprint_notice(level, &text),
            // Transient turn progress (reasoning, tool steps, compaction
            // status, observer narration, the planning checklist) and
            // out-of-band media (attachments) aren't part of the one-shot
            // text answer.
            AgentEvent::Reasoning(_)
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolCompleted { .. }
            | AgentEvent::Status(_)
            | AgentEvent::Progress(_)
            | AgentEvent::TaskList(_)
            | AgentEvent::Attachment(_) => {}
        }
    }
    writer.finish_incomplete()
}

/// In-process [`ConnectionSink`] — the gateway's `GatewaySink` minus the
/// wire-frame translation: it forwards [`SessionEvent`]s straight to an
/// mpsc the prompt consumer drains.
struct InProcessSink {
    event_tx: mpsc::Sender<SessionEvent>,
}

impl ConnectionSink for InProcessSink {
    fn try_send_event(&self, event: SessionEvent) -> SendOutcome {
        match self.event_tx.try_send(event) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    fn try_send_frame(&self, _frame: aura_channels::wire::Frame) -> SendOutcome {
        // Control frames (Reset / SessionActivity / …) aren't needed for a
        // one-shot turn; the consumer reads SessionEvents only.
        SendOutcome::Sent
    }
}

/// Auto-approve gate for `--dangerously-allow-all`. The `Deny` default is
/// covered by [`AutoDenyGate`]; this is its opt-in opposite.
struct AutoApproveGate;

#[async_trait::async_trait]
impl ApprovalGate for AutoApproveGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

// --------------------------------------------------------------------------
// Shared output rendering + helpers
// --------------------------------------------------------------------------

/// Streams answer text to stdout (text mode) or buffers it (JSON mode),
/// reconciling streamed deltas against the canonical final Message so the
/// final text isn't printed on top of what already streamed. Mirrors the
/// TUI's `finalize_lines` contract.
struct AnswerWriter {
    answer: String,
    /// Whether any delta has been written to stdout.
    printed: bool,
    json: bool,
}

impl AnswerWriter {
    fn new(json: bool) -> Self {
        Self {
            answer: String::new(),
            printed: false,
            json,
        }
    }

    fn delta(&mut self, text: &str) {
        self.answer.push_str(text);
        if !self.json {
            self.printed = true;
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }

    /// Reconcile against the turn's final, canonical response text.
    fn finalize(&mut self, final_text: &str) {
        if !self.json {
            if self.printed {
                if !self.answer.ends_with('\n') {
                    println!();
                }
            } else if !final_text.is_empty() {
                // No deltas streamed — some producers deliver only a final
                // Message; render it now.
                println!("{final_text}");
            }
        }
        if !final_text.is_empty() {
            self.answer = final_text.to_owned();
        }
    }

    fn into_answer(self) -> String {
        self.answer
    }

    /// The event source ended without a final response. Keep streamed text
    /// if any arrived; otherwise it's an error.
    fn finish_incomplete(self) -> anyhow::Result<String> {
        if self.answer.is_empty() {
            anyhow::bail!("the turn ended before producing a response");
        }
        if !self.json && self.printed && !self.answer.ends_with('\n') {
            println!();
        }
        Ok(self.answer)
    }
}

fn approval_decision(allow_all: bool) -> ApprovalDecision {
    if allow_all {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Deny
    }
}

fn note_approval(decision: ApprovalDecision, tool: &str) {
    if matches!(decision, ApprovalDecision::Deny) {
        warn!(%tool, "tool approval auto-denied; pass --dangerously-allow-all to allow");
    } else {
        debug!(%tool, "tool approval auto-approved");
    }
}

fn eprint_notice(level: NoticeLevel, text: &str) {
    eprintln!("[{level:?}] {text}");
}

fn resolve_session_id(session: Option<&str>) -> SessionId {
    SessionId::from(
        session
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
    )
}

fn emit_json(session_id: &SessionId, answer: &str) {
    let out = serde_json::json!({
        "session_id": session_id.to_string(),
        "response": answer,
    });
    println!("{out}");
}

/// Failure counterpart of [`emit_json`]: the (attempted) session id plus
/// the error chain, so a `--json` consumer gets a parseable object on
/// stdout whether the turn succeeded or not. `{err:#}` renders the full
/// `anyhow` cause chain on one line.
fn emit_json_error(session_id: &SessionId, err: &anyhow::Error) {
    let out = serde_json::json!({
        "session_id": session_id.to_string(),
        "error": format!("{err:#}"),
    });
    println!("{out}");
}

/// Build the inbound user message for the pinned session. Mirrors the
/// TUI's `dispatch_user_message`: text-only content on the built-in `tui`
/// channel.
fn build_incoming(session_id: &SessionId, prompt: &str) -> IncomingMessage {
    IncomingMessage {
        message: Message {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            channel: ChannelType::tui(),
            sender: cli_user(),
            content: vec![ContentBlock::Text(prompt.to_owned())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
        platform_msg_id: String::new(),
    }
}

/// The local operator identity for a CLI turn. Same `$USER`/`$USERNAME`
/// derivation the TUI uses, so `prompt` and `tui` turns share a user id.
fn cli_user() -> User {
    let id = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "aura-cli".to_string());
    User {
        id,
        name: None,
        channel: ChannelType::tui(),
    }
}

/// Concatenate the text blocks of a response, ignoring non-text (media)
/// blocks — `aura prompt` is a text surface.
fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_channels::{AgentOutput, OutgoingMessage, Result as ChannelResult};

    fn text_blocks(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.to_owned())]
    }

    // ---- gateway (WS) path ------------------------------------------------

    fn ws_stream(events: Vec<TransportEvent>) -> TransportEventStream {
        let items: Vec<ChannelResult<TransportEvent>> = events.into_iter().map(Ok).collect();
        Box::pin(futures::stream::iter(items))
    }

    #[tokio::test]
    async fn ws_final_message_is_canonical_answer() {
        let q = ApprovalQueue::new();
        let mut s = ws_stream(vec![
            TransportEvent::StreamDelta("hel".into()),
            TransportEvent::StreamDelta("lo".into()),
            TransportEvent::Response(text_blocks("hello")),
        ]);
        let out = consume_ws_turn(&q, &mut s, ApprovalDecision::Deny, true)
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn ws_response_without_deltas_is_returned() {
        let q = ApprovalQueue::new();
        let mut s = ws_stream(vec![TransportEvent::Response(text_blocks("only final"))]);
        let out = consume_ws_turn(&q, &mut s, ApprovalDecision::Deny, true)
            .await
            .unwrap();
        assert_eq!(out, "only final");
    }

    #[tokio::test]
    async fn ws_deltas_without_final_response_fall_back() {
        let q = ApprovalQueue::new();
        let mut s = ws_stream(vec![TransportEvent::StreamDelta("partial".into())]);
        let out = consume_ws_turn(&q, &mut s, ApprovalDecision::Deny, true)
            .await
            .unwrap();
        assert_eq!(out, "partial");
    }

    #[tokio::test]
    async fn ws_empty_turn_errors() {
        let q = ApprovalQueue::new();
        let mut s = ws_stream(vec![]);
        assert!(
            consume_ws_turn(&q, &mut s, ApprovalDecision::Deny, true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ws_approval_is_auto_resolved_from_queue() {
        let q = ApprovalQueue::new();
        q.enqueue_mirror(ApprovalRequest {
            call_id: "c1".into(),
            session_id: SessionId::from("s1"),
            user_id: String::new(),
            tool: "BashTool".into(),
            accesses: Vec::new(),
            params_preview: String::new(),
            description: None,
        });
        let mut s = ws_stream(vec![
            TransportEvent::ApprovalRequested,
            TransportEvent::Response(text_blocks("done")),
        ]);
        let out = consume_ws_turn(&q, &mut s, ApprovalDecision::Deny, true)
            .await
            .unwrap();
        assert_eq!(out, "done");
        assert!(q.is_empty(), "approval entry should be resolved and popped");
    }

    // ---- in-process path --------------------------------------------------

    fn agent_event(event: AgentEvent) -> SessionEvent {
        SessionEvent::Agent(AgentOutput {
            session_id: SessionId::from("s1"),
            user_id: String::new(),
            channel: ChannelType::tui(),
            event,
        })
    }

    fn outgoing(s: &str) -> OutgoingMessage {
        OutgoingMessage {
            session_id: SessionId::from("s1"),
            user_id: String::new(),
            channel: ChannelType::tui(),
            content: text_blocks(s),
            reply_to: None,
            metadata: MessageMetadata::default(),
            ordinal: None,
        }
    }

    async fn drive_inprocess(events: Vec<SessionEvent>, json: bool) -> anyhow::Result<String> {
        let (tx, mut rx) = mpsc::channel(16);
        for e in events {
            tx.send(e).await.unwrap();
        }
        drop(tx);
        consume_inprocess_turn(&mut rx, json).await
    }

    #[tokio::test]
    async fn inprocess_streams_then_finalizes() {
        let out = drive_inprocess(
            vec![
                agent_event(AgentEvent::AnswerDelta("hel".into())),
                agent_event(AgentEvent::AnswerDelta("lo".into())),
                agent_event(AgentEvent::Message(outgoing("hello"))),
            ],
            true,
        )
        .await
        .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn inprocess_message_only() {
        let out = drive_inprocess(
            vec![agent_event(AgentEvent::Message(outgoing("only")))],
            true,
        )
        .await
        .unwrap();
        assert_eq!(out, "only");
    }

    #[tokio::test]
    async fn inprocess_ignores_non_agent_events() {
        // A trailing user echo / non-agent event must not be mistaken for
        // the answer; the final Message still wins.
        let out = drive_inprocess(
            vec![
                agent_event(AgentEvent::AnswerDelta("hi".into())),
                agent_event(AgentEvent::Message(outgoing("hi there"))),
            ],
            true,
        )
        .await
        .unwrap();
        assert_eq!(out, "hi there");
    }

    #[tokio::test]
    async fn inprocess_incomplete_errors() {
        assert!(drive_inprocess(vec![], true).await.is_err());
    }

    // ---- turn timeout -----------------------------------------------------

    #[tokio::test]
    async fn await_turn_errors_when_reply_never_arrives() {
        // Models a pre-response failure: the consume future never resolves
        // (the runtime rejected the turn and dispatched nothing). The
        // budget must convert that hang into an error.
        let out = await_turn(
            Some(Duration::from_millis(10)),
            std::future::pending::<anyhow::Result<String>>(),
        )
        .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn await_turn_passes_through_without_a_budget() {
        let out = await_turn(None, async { Ok("hi".to_string()) })
            .await
            .unwrap();
        assert_eq!(out, "hi");
    }
}
