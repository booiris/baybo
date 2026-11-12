use std::sync::Arc;

use baybo_channels::{AgentOutput, IncomingMessage, Message};
use baybo_cost::CostManager;
use baybo_llm::TokenUsage;
use baybo_model::{
    ChannelType, ChatMessage, ContentBlock, ExternalAgentKind, Lineage, LineageKind,
    MessageMetadata, SUBAGENT_CHANNEL_TAG, Session, SessionId, SpanId, SubagentBackend,
    SubagentExitStatus, SubagentParentContext, SubagentResult, SubagentSpawnRequest, TriggerKind,
    TurnId, User,
};
use baybo_session::SessionManager;
use baybo_subagent::{SpawnedSubagent, SubagentRun};
use baybo_turn::{CancelReason, TurnInput, TurnLifecycle, TurnOutput};
use baybo_workspace::WorkspacePaths;
use chrono::Utc;
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::actor::router::{ActorSpawner, build_oneshot_actor};
use crate::actor::subagent::await_subagent_terminal;
use crate::external_agent::{
    EXTERNAL_SUBAGENT_TIMEOUT, ExternalAgent, ExternalAgentEvent, ExternalAgentRegistry,
    ExternalAgentRequest,
};
use crate::runtime::llm_pool::LlmPoolHandle;

/// `output_tx` buffer for a subagent's actor. Intentionally smaller than
/// the operator-configured channel size for top-level actors — a child
/// session only emits its final `AgentEvent::Message` (deltas are not
/// forwarded back through this channel), so 64 is overkill but matches
/// the wait routine's earlier sizing.
const SUBAGENT_OUTPUT_BUFFER: usize = 64;

/// Construction bundle for [`ActorSubagentSpawner`] — every field is
/// required; call sites populate it via struct literal.
pub struct SubagentSpawnerConfig {
    pub session_manager: Arc<SessionManager>,
    pub dispatch_limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter>,
    pub cost_manager: Arc<CostManager>,
    pub turn_lifecycle: Arc<TurnLifecycle>,
    pub actor_parent_token: CancellationToken,
    pub external_agents: Arc<ExternalAgentRegistry>,
    pub llm_pool: LlmPoolHandle,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub actor_spawner: ActorSpawner,
}

/// Actor-backed [`baybo_subagent::SubagentSpawner`]: materialises a child
/// `AgentActor` (or routes to an external backend) for each
/// `spawn_subagent` call. Lifted out of the router so the tool reaches it
/// directly — there is no cross-actor channel. The child's agent loop
/// still self-drives on its own task; `actor_spawner` just hands back a
/// mailbox.
pub struct ActorSubagentSpawner {
    session_manager: Arc<SessionManager>,
    dispatch_limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter>,
    cost_manager: Arc<CostManager>,
    turn_lifecycle: Arc<TurnLifecycle>,
    actor_parent_token: CancellationToken,
    external_agents: Arc<ExternalAgentRegistry>,
    llm_pool: LlmPoolHandle,
    workspace_paths: Arc<WorkspacePaths>,
    actor_spawner: ActorSpawner,
}

impl ActorSubagentSpawner {
    pub fn from_config(config: SubagentSpawnerConfig) -> Self {
        let SubagentSpawnerConfig {
            session_manager,
            dispatch_limiter,
            cost_manager,
            turn_lifecycle,
            actor_parent_token,
            external_agents,
            llm_pool,
            workspace_paths,
            actor_spawner,
        } = config;
        Self {
            session_manager,
            dispatch_limiter,
            cost_manager,
            turn_lifecycle,
            actor_parent_token,
            external_agents,
            llm_pool,
            workspace_paths,
            actor_spawner,
        }
    }
}

#[async_trait::async_trait]
impl baybo_subagent::SubagentSpawner for ActorSubagentSpawner {
    async fn spawn(
        &self,
        parent_ctx: SubagentParentContext,
        request: SubagentSpawnRequest,
    ) -> SpawnedSubagent {
        let parent_session_id = parent_ctx.session_id.clone();
        // `fan_out_root` is `None` only on synthesized test requests
        // (those tests don't gate the limiter, so the release is a
        // no-op). Production spawns always carry a root because the
        // tool reserved a slot before calling.
        let fan_out_root = request.fan_out_root.clone();
        let parent = match self.session_manager.get(&parent_session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return self.failed(
                    &fan_out_root,
                    format!("parent session {parent_session_id} not found"),
                );
            }
            Err(e) => return self.failed(&fan_out_root, format!("load parent session: {e}")),
        };

        let spawned = match request.backend.clone() {
            SubagentBackend::Baybo => self.spawn_baybo_subagent(parent, parent_ctx, request).await,
            SubagentBackend::External { external_kind } => {
                self.spawn_external_subagent(parent, parent_ctx, request, external_kind)
                    .await
            }
        };
        match spawned {
            SpawnedSubagent::Failed(result) => {
                self.release_fan_out_slot(&fan_out_root);
                SpawnedSubagent::Failed(result)
            }
            SpawnedSubagent::Running(run) => SpawnedSubagent::Running(holding_slot(
                run,
                Arc::clone(&self.dispatch_limiter),
                fan_out_root,
            )),
        }
    }
}

impl ActorSubagentSpawner {
    fn failed(&self, fan_out_root: &Option<SessionId>, reason: String) -> SpawnedSubagent {
        self.release_fan_out_slot(fan_out_root);
        SpawnedSubagent::Failed(SubagentResult::failed(reason))
    }

    /// The token a child's work hangs off. A child the parent may hand to the
    /// background must outlive the dispatching turn — the turn that emitted
    /// `spawn_subagent` ends as soon as the tool returns — so it anchors to
    /// the process-wide token (shutdown still cascades). Otherwise it stays
    /// on the parent's per-turn scope. Either way the parent stops it through
    /// the run's own `cancel` while it waits in the foreground.
    fn anchor(&self, parent_ctx: &SubagentParentContext) -> CancellationToken {
        if parent_ctx.background_eligible {
            self.actor_parent_token.clone()
        } else {
            parent_ctx.cancel_token.clone()
        }
    }

    /// In-process backend: spawn a full `AgentActor` for the child, fresh or
    /// resuming a prior Baybo child.
    async fn spawn_baybo_subagent(
        &self,
        parent: Session,
        parent_ctx: SubagentParentContext,
        request: SubagentSpawnRequest,
    ) -> SpawnedSubagent {
        let effective_parent_token = self.anchor(&parent_ctx);
        let SubagentParentContext {
            turn_id: parent_turn_id,
            span_id: parent_span_id,
            ..
        } = parent_ctx;
        let child_session = match self
            .resolve_child_session(
                &parent,
                parent_turn_id,
                parent_span_id,
                &request,
                baybo_model::SubagentBackendKind::Baybo,
            )
            .await
        {
            Ok(s) => s,
            Err(failed) => return SpawnedSubagent::Failed(failed),
        };

        // Subscribe to terminal events BEFORE the build+dispatch so a child
        // that exits synchronously can't slip its terminal past us.
        let terminal_rx = self.turn_lifecycle.subscribe_lifecycle_events();

        let now = Utc::now();
        let incoming = IncomingMessage {
            message: Message {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: child_session.id.clone(),
                channel: child_session.channel.clone(),
                sender: child_session.user.clone(),
                content: vec![ContentBlock::Text(request.prompt.clone())],
                timestamp: now,
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
            platform_msg_id: String::new(),
            bot_id: String::new(),
        };
        let child_session_id = child_session.id.clone();
        // Tier resolution: `model_tier` lookup, falling through to the
        // pool default (handled inside the spawner closure when `None`
        // is passed). The tool already merged the profile's default_tier
        // into `model_tier`, so this is the final step.
        let llm = request
            .model_tier
            .and_then(|t| self.llm_pool.read().resolve_tier(t));

        let (output_tx, output_rx) = mpsc::channel::<AgentOutput>(SUBAGENT_OUTPUT_BUFFER);
        let (mailbox, actor_token) = build_oneshot_actor(
            &self.actor_spawner,
            &effective_parent_token,
            child_session,
            llm,
            // Subagents pin an ENTRY (via model_tier); the candidate-model
            // and reasoning-effort dimensions are chat-only picks, so a
            // subagent always runs the entry's default model + effort.
            None,
            None,
            output_tx,
        );

        // The prompt (`SubagentSpawned`) kicks the parked child actor into
        // real work, then the run waits on its terminal.
        let turn_lifecycle = Arc::clone(&self.turn_lifecycle);
        let terminal = {
            let child_session_id = child_session_id.clone();
            let actor_token = actor_token.clone();
            async move {
                if let Err(e) = mailbox
                    .send(AgentMessage::SubagentSpawned {
                        initial_message: Box::new(incoming),
                        parent_turn_id,
                    })
                    .await
                {
                    return SubagentResult::failed(format!("dispatch child input: {e}"));
                }
                await_subagent_terminal(
                    child_session_id,
                    output_rx,
                    terminal_rx,
                    mailbox,
                    actor_token,
                    turn_lifecycle,
                )
                .await
            }
            .boxed()
        };
        SpawnedSubagent::Running(SubagentRun {
            child_session_id,
            cancel: actor_token,
            terminal,
        })
    }

    /// External backend: route one-shot delegation to a registered
    /// external-agent impl (claude_cli, codex_cli). No `AgentActor` is
    /// built; the agent's event stream is driven to a terminal result.
    async fn spawn_external_subagent(
        &self,
        parent: Session,
        parent_ctx: SubagentParentContext,
        request: SubagentSpawnRequest,
        kind: ExternalAgentKind,
    ) -> SpawnedSubagent {
        let anchor = self.anchor(&parent_ctx);
        let SubagentParentContext {
            turn_id: parent_turn_id,
            span_id: parent_span_id,
            ..
        } = parent_ctx;
        let Some(agent) = self.external_agents.get(kind) else {
            return SpawnedSubagent::Failed(SubagentResult::failed(format!(
                "external agent {:?} is not registered: no `{}` binary was found on this host's \
                 PATH at startup, or it is disabled in `external_agents.{}.enabled`. Install it \
                 (restart baybo if you just did) or use backend='baybo'. \
                 `baybo external-agent status` re-probes and reports the reason.",
                kind.as_str(),
                kind.binary_name(),
                kind.as_str(),
            )));
        };

        let child_session = match self
            .resolve_child_session(
                &parent,
                parent_turn_id,
                parent_span_id,
                &request,
                baybo_model::SubagentBackendKind::External(kind),
            )
            .await
        {
            Ok(s) => s,
            Err(failed) => return SpawnedSubagent::Failed(failed),
        };
        let (dir_name, resume_key) = match &child_session.state.subagent_backend {
            Some(baybo_model::SubagentBackendTag::External {
                workspace_dir,
                resume_key,
                ..
            }) => (workspace_dir.clone(), resume_key.clone()),
            _ => (child_session.id.as_ref().to_string(), None),
        };

        let workspace_dir = self
            .workspace_paths
            .work_dir()
            .join(kind.as_str())
            .join(dir_name);

        let cancel = anchor.child_token();
        let child_session_id = child_session.id.clone();
        let turn_ctx = ExternalTurnCtx {
            lifecycle: Arc::clone(&self.turn_lifecycle),
            cost_manager: Arc::clone(&self.cost_manager),
            user_id: child_session.user.id.clone(),
            trigger_kind: child_session.trigger.kind(),
            parent_turn_id,
        };
        let external_request = ExternalAgentRequest {
            task: request.prompt.clone(),
            workspace_dir,
            resume_key,
            cancel: cancel.clone(),
            timeout: EXTERNAL_SUBAGENT_TIMEOUT,
        };
        let terminal = run_external_agent_turn(
            agent,
            kind,
            external_request,
            child_session_id.clone(),
            Arc::clone(&self.session_manager),
            turn_ctx,
        )
        .boxed();
        SpawnedSubagent::Running(SubagentRun {
            child_session_id,
            cancel,
            terminal,
        })
    }

    fn release_fan_out_slot(&self, root: &Option<SessionId>) {
        release_reserved_slot(self.dispatch_limiter.as_ref(), root);
    }

    /// Returns the child Session — loading an existing one when
    /// `resume_session_id` is set, otherwise minting a new
    /// `LineageKind::Subagent` row. Backend-mismatch + parent +
    /// hidden + lineage checks all run on the resume path.
    async fn resolve_child_session(
        &self,
        parent: &Session,
        parent_turn_id: TurnId,
        parent_span_id: SpanId,
        request: &SubagentSpawnRequest,
        backend: baybo_model::SubagentBackendKind,
    ) -> Result<Session, SubagentResult> {
        if let Some(resume_id) = request.resume_session_id.as_ref() {
            let child = match self.session_manager.get(resume_id).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    return Err(SubagentResult::failed(format!(
                        "resume_session_id {resume_id:?} not found"
                    )));
                }
                Err(e) => {
                    return Err(SubagentResult::failed(format!(
                        "resume_session_id load failed: {e}"
                    )));
                }
            };
            if let Err(reason) =
                validate_resume_session(&child, parent, backend, &request.subagent_type)
            {
                return Err(SubagentResult::failed(reason));
            }
            Ok(child)
        } else {
            let child_channel = ChannelType::from(SUBAGENT_CHANNEL_TAG);
            let child_user = User {
                id: parent.user.id.clone(),
                name: parent.user.name.clone(),
                channel: child_channel.clone(),
            };
            let lineage = Lineage {
                parent_session_id: parent.id.clone(),
                parent_turn_id,
                parent_span_id: Some(parent_span_id),
                kind: LineageKind::Subagent,
            };
            let mut child = self
                .session_manager
                .create_spawned_session(child_user, child_channel, parent, lineage)
                .await
                .map_err(|e| SubagentResult::failed(format!("create child session: {e}")))?;
            // workspace_dir is identity, not per-call state: an external
            // subagent works under `<root>/work/<backend>/<child_id>/`,
            // resolved once here from the child session id and reused
            // verbatim on every resume.
            child.state.subagent_backend = Some(match backend {
                baybo_model::SubagentBackendKind::Baybo => baybo_model::SubagentBackendTag::Baybo,
                baybo_model::SubagentBackendKind::External(kind) => {
                    baybo_model::SubagentBackendTag::External {
                        external_kind: kind,
                        workspace_dir: child.id.as_ref().to_string(),
                        resume_key: None,
                    }
                }
            });
            child.state.subagent_type = Some(request.subagent_type.clone());
            child.state.tool_allowlist = request.tool_allowlist.clone();
            if let Err(e) = self.session_manager.store().save(&child).await {
                return Err(SubagentResult::failed(format!(
                    "persist subagent identity on {}: {e}",
                    child.id,
                )));
            }
            // The only place the task text reaches the session row: it exists
            // otherwise solely inside the spawn request and the trace span
            // label, so the surface that lists a parent's children
            // (`/v1/chat/sessions/{id}/subagents`) would have nothing to name
            // them by.
            //
            // Through the SETTER, not the struct field: `save`'s DO UPDATE
            // deliberately omits `title` (it belongs to the targeted setters so
            // a stale in-memory `Session` cannot clobber a rename), and
            // `create_spawned_session` has already inserted the row — so
            // assigning `child.title` here would be silently dropped.
            // Best-effort: a child with no title still lists under its profile
            // name, which is not worth failing a spawn over.
            child.title = Some(request.task_summary.clone());
            if let Err(e) = self
                .session_manager
                .store()
                .set_title_if_absent(&child.id, &request.task_summary)
                .await
            {
                warn!(
                    session_id = %child.id,
                    error = %e,
                    "failed to stamp subagent task title",
                );
            }
            Ok(child)
        }
    }
}

/// `run`, holding its fan-out slot until the child's run ends, whichever way
/// the parent ends up waiting for it. `background::run` drives `terminal` to
/// its end on its own task, so the release happens even when the parent's
/// tool call is abandoned.
fn holding_slot(
    run: SubagentRun,
    limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter>,
    fan_out_root: Option<SessionId>,
) -> SubagentRun {
    let terminal = run.terminal;
    SubagentRun {
        terminal: async move {
            let result = terminal.await;
            release_reserved_slot(limiter.as_ref(), &fan_out_root);
            result
        }
        .boxed(),
        ..run
    }
}

/// Release a reserved fan-out slot for `root`, if any. A free fn so the
/// run's `terminal` wrapper — which captures only an
/// `Arc<dyn SubagentDispatchLimiter>`, not `&self` — shares the `&self`
/// method's single implementation.
fn release_reserved_slot(
    limiter: &dyn baybo_subagent::SubagentDispatchLimiter,
    root: &Option<SessionId>,
) {
    if let Some(id) = root {
        limiter.release(id);
    }
}

fn validate_resume_session(
    child: &Session,
    parent: &Session,
    backend: baybo_model::SubagentBackendKind,
    request_subagent_type: &str,
) -> Result<(), String> {
    let resume_id = &child.id;
    if child.hidden {
        return Err(format!(
            "resume_session_id {resume_id:?} was hidden — its conversation is no longer available"
        ));
    }
    let Some(lineage) = child.lineage.as_ref() else {
        return Err(format!(
            "resume_session_id {resume_id:?} is not a subagent session"
        ));
    };
    if lineage.parent_session_id != parent.id {
        return Err(format!(
            "resume_session_id {resume_id:?} belongs to a different parent session"
        ));
    }
    if !matches!(lineage.kind, LineageKind::Subagent) {
        return Err(format!(
            "resume_session_id {resume_id:?} is not a Subagent lineage"
        ));
    }
    let stored = child.state.subagent_backend.as_ref().ok_or_else(|| {
        format!(
            "resume_session_id {resume_id:?} has no recorded subagent_backend — either it's a \
             pre-tag session that predates the durable backend field, or the row was created \
             outside the spawn router. Refusing to resume."
        )
    })?;
    if stored.kind() != backend {
        return Err(format!(
            "resume_session_id {resume_id:?} was created with backend={}; cannot resume as backend={}",
            stored.kind().label(),
            backend.label(),
        ));
    }
    // Profile identity is pinned at genesis: a child spawned as one
    // `subagent_type` can't be resumed as another, which would run a
    // different profile's prompt/contract over the existing transcript.
    // `None` (pre-pin rows) can't be checked.
    if let Some(stored_type) = child.state.subagent_type.as_deref()
        && stored_type != request_subagent_type
    {
        return Err(format!(
            "resume_session_id {resume_id:?} was spawned as subagent_type {stored_type:?}; \
             cannot resume it as {request_subagent_type:?} (different profile)"
        ));
    }
    // External resume requires the persisted resume_key. If `None`,
    // the prior call never reached its init event — resuming would
    // start a fresh conversation under the existing child_session_id.
    if let baybo_model::SubagentBackendTag::External {
        external_kind,
        resume_key: None,
        ..
    } = stored
    {
        return Err(format!(
            "resume_session_id {resume_id:?} has no persisted {} resume_key — the prior call \
             never produced one. Spawn a fresh subagent instead.",
            external_kind.as_str(),
        ));
    }
    Ok(())
}

/// Turn-lifecycle metadata for an external subagent run. The child
/// session needs its own `Spawned` turn for the trace browser to
/// surface it — `list_session_summaries` drops zero-turn sessions and
/// `get_trace` 404s on them.
struct ExternalTurnCtx {
    lifecycle: Arc<TurnLifecycle>,
    cost_manager: Arc<CostManager>,
    user_id: String,
    trigger_kind: TriggerKind,
    parent_turn_id: TurnId,
}

/// Wrap [`run_external_agent`] in a `Spawned` turn so the external
/// subagent's child session is visible/inspectable in the trace
/// browser, mirroring how the in-process Baybo backend's actor creates
/// a turn per turn. The terminal `SubagentExitStatus` maps onto the
/// turn's terminal transition (Completed→complete, Failed→fail,
/// Timeout→cancel(SubagentTimeout), Cancelled→cancel(ParentCancelled)).
///
/// Uses the `TurnLifecycle` primitives directly rather than
/// `scope::with_turn` because `with_turn` collapses every non-success
/// exit into `fail()`; the subagent contract needs the distinct
/// `SubagentTimeout` / `ParentCancelled` cancel reasons preserved.
/// Terminal-race transitions (operator cancel landing as the run
/// completes) surface as `InvalidTransition`, swallowed via `let _`.
async fn run_external_agent_turn(
    agent: Arc<dyn ExternalAgent>,
    kind: ExternalAgentKind,
    request: ExternalAgentRequest,
    child_session_id: SessionId,
    session_manager: Arc<baybo_session::SessionManager>,
    turn_ctx: ExternalTurnCtx,
) -> SubagentResult {
    let cancel = request.cancel.clone();
    let initial_prompt = vec![ContentBlock::Text(request.task.clone())];
    let turn = match turn_ctx
        .lifecycle
        .start_turn(
            child_session_id.clone(),
            turn_ctx.trigger_kind,
            TurnInput::Spawned { initial_prompt },
            Some(turn_ctx.parent_turn_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Failed {
                    reason: format!("start subagent turn: {e}"),
                },
            };
        }
    };
    // Register the cancel token so an operator-issued turn cancel trips
    // the run, then move Pending → InProgress.
    let _cancel_guard = turn_ctx.lifecycle.register_running(turn.id, cancel);
    if let Err(e) = turn_ctx.lifecycle.start(&turn.id).await {
        let _ = turn_ctx
            .lifecycle
            .fail(&turn.id, format!("start: {e}"))
            .await;
        return SubagentResult {
            child_session_id,
            final_content: None,
            status: SubagentExitStatus::Failed {
                reason: format!("start subagent turn: {e}"),
            },
        };
    }

    let (result, token_usage) =
        run_external_agent(agent, kind, request, child_session_id, session_manager).await;

    // Subscription-billed: log token consumption with cost $0 so the
    // analytics per-session/model breakdowns include external runs
    // without charging the operator's budget. No span tree exists, so
    // the record is keyed to a nil span id.
    if let Some(usage) = token_usage {
        turn_ctx.cost_manager.record_external_tokens(
            &turn_ctx.user_id,
            turn.session_id.clone(),
            turn.id,
            SpanId::default(),
            baybo_llm::CallReason::Chat,
            &format!("{} (external agent)", kind.as_str()),
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        );
    }

    match &result.status {
        SubagentExitStatus::Completed => {
            let content = result.final_content.clone().unwrap_or_default();
            let _ = turn_ctx
                .lifecycle
                .complete(
                    &turn.id,
                    // Subagent (Spawned) output — not a user turn, never pushed.
                    TurnOutput::Message {
                        content,
                        ordinal: None,
                    },
                )
                .await;
        }
        SubagentExitStatus::Failed { reason } => {
            let _ = turn_ctx.lifecycle.fail(&turn.id, reason.clone()).await;
        }
        SubagentExitStatus::Timeout | SubagentExitStatus::ForegroundWaitElapsed => {
            let _ = turn_ctx
                .lifecycle
                .cancel(&turn.id, CancelReason::SubagentTimeout, vec![])
                .await;
        }
        SubagentExitStatus::Cancelled => {
            let _ = turn_ctx
                .lifecycle
                .cancel(&turn.id, CancelReason::ParentCancelled, vec![])
                .await;
        }
    }

    result
}

/// Drives the agent stream → persists transcript turns, captures the
/// terminal status, and returns any `Usage` the agent reported (for
/// the caller to log into `cost_records`). Stays turn-agnostic;
/// [`run_external_agent_turn`] owns the surrounding turn lifecycle.
async fn run_external_agent(
    agent: Arc<dyn ExternalAgent>,
    kind: ExternalAgentKind,
    request: ExternalAgentRequest,
    child_session_id: SessionId,
    session_manager: Arc<baybo_session::SessionManager>,
) -> (SubagentResult, Option<TokenUsage>) {
    // Persist the operator-supplied task up-front so the transcript
    // shows something even if the external agent crashes before
    // emitting its first event.
    if let Err(error) = append_subagent_message(
        &session_manager,
        &child_session_id,
        // The task is the parent agent's instruction to the subagent, not a
        // human channel input — but it IS the child's opening errand, and the
        // one agent-injected row its transcript should render (SubagentSeed,
        // same as the baybo-backend path's `append_spawned_prompt`).
        ChatMessage::subagent_seed(vec![ContentBlock::Text(request.task.clone())]),
    )
    .await
    {
        return (
            SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Failed {
                    reason: format!("persist external agent task: {error}"),
                },
            },
            None,
        );
    }

    let cancel = request.cancel.clone();
    let mut stream = match agent.run(request).await {
        Ok(s) => s,
        Err(e) => {
            return (
                SubagentResult {
                    child_session_id,
                    final_content: None,
                    status: SubagentExitStatus::Failed {
                        reason: format!("external agent run: {e}"),
                    },
                },
                None,
            );
        }
    };

    let mut final_content: Option<Vec<ContentBlock>> = None;
    let mut final_status: Option<SubagentExitStatus> = None;
    let mut token_usage: Option<TokenUsage> = None;

    // The parser inside the agent already enforces the timeout against
    // its own deadline. Adding a second wall-clock deadline here would
    // (a) double the effective budget on cancel/timeout and (b) fire
    // during long reasoning pauses where the parser is correctly idle.
    // The cancel token is the only outer signal we need.
    //
    // Parsers defer `FinalContent` (and `Usage`) until after the child
    // is reaped, so breaking on `FinalContent` does not drop the stream
    // mid-cleanup. On a non-`FinalContent` exit we still drain the
    // remaining events below to give the parser a chance to reap.
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                final_status = Some(SubagentExitStatus::Cancelled);
                break;
            }
            n = stream.next() => n,
        };
        match next {
            Some(Ok(ExternalAgentEvent::TextDelta(_))) => {
                // Deltas not forwarded to the parent yet.
            }
            Some(Ok(ExternalAgentEvent::Usage(usage))) => {
                // Logged into cost_records (cost $0, tokens only) by
                // run_external_agent_turn after the stream closes.
                token_usage = Some(usage);
            }
            Some(Ok(ExternalAgentEvent::ResumeKey(key))) => {
                if let Err(e) =
                    persist_resume_key(&session_manager, &child_session_id, kind, &key).await
                {
                    warn!(
                        session_id = %child_session_id,
                        kind = %kind.as_str(),
                        error = %e,
                        "failed to persist resume_key; --resume will not work on the next call",
                    );
                }
            }
            Some(Ok(ExternalAgentEvent::Intermediate(msg))) => {
                if let Err(error) =
                    append_subagent_message(&session_manager, &child_session_id, msg).await
                {
                    final_status = Some(SubagentExitStatus::Failed {
                        reason: format!("persist external agent turn: {error}"),
                    });
                    break;
                }
            }
            Some(Ok(ExternalAgentEvent::FinalContent(blocks))) => {
                final_content = Some(blocks);
                final_status = Some(SubagentExitStatus::Completed);
                break;
            }
            Some(Err(e)) => {
                let msg = e.to_string();
                final_status = Some(if msg.contains("idle timeout") {
                    SubagentExitStatus::Timeout
                } else {
                    SubagentExitStatus::Failed { reason: msg }
                });
                break;
            }
            None => {
                if final_status.is_none() {
                    final_status = Some(SubagentExitStatus::Failed {
                        reason: "external agent stream ended without FinalContent".into(),
                    });
                }
                break;
            }
        }
    }

    // Drain any residual events so the parser finishes its `reap_after_stream_close`
    // path naturally. Without this, dropping the stream here would trigger
    // `kill_on_drop` and skip the parser's exit-status check.
    while stream.next().await.is_some() {}

    // The FinalContent text duplicates the last `Intermediate(Assistant)`
    // event we already persisted, so don't write it again here.

    (
        SubagentResult {
            child_session_id,
            final_content,
            status: final_status.unwrap_or(SubagentExitStatus::Completed),
        },
        token_usage,
    )
}

async fn append_subagent_message(
    session_manager: &Arc<baybo_session::SessionManager>,
    session_id: &SessionId,
    mut msg: ChatMessage,
) -> anyhow::Result<()> {
    cap_external_agent_blocks(&mut msg);
    session_manager
        .append_session_message(session_id, &msg)
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
}

/// Bound the text an external agent's turn writes into `session_messages`.
///
/// The main agent loop routes every tool result through
/// `ContextManager::cap_tool_output`, which caps at `MAX_TOOL_OUTPUT_BYTES` and
/// spills the remainder to a file. The external-agent leg writes straight to
/// the store and never passes through that path, so a child that read a large
/// file persisted the whole thing as a single transcript row — the cap had a
/// hole exactly the width of this function.
///
/// It matters more here than in a trace span: session rows are core data, never
/// rewritten and never deleted, so an unbounded row is permanent. The bound is
/// prospective only — existing rows stay exactly as they are.
fn cap_external_agent_blocks(msg: &mut ChatMessage) {
    use baybo_model::MAX_TOOL_OUTPUT_BYTES;

    for block in msg.content.iter_mut() {
        match block {
            ContentBlock::Text(s) => cap_in_place(s, MAX_TOOL_OUTPUT_BYTES),
            ContentBlock::ToolResult { content, .. } => {
                cap_in_place(content, MAX_TOOL_OUTPUT_BYTES)
            }
            // Media blocks carry a `BlobRef`, not bytes.
            _ => {}
        }
    }
}

fn cap_in_place(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let total = s.len();
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    use std::fmt::Write as _;
    let _ = write!(s, "\n\n[... truncated: {cut}/{total} bytes shown]");
}

async fn persist_resume_key(
    session_manager: &Arc<baybo_session::SessionManager>,
    session_id: &SessionId,
    kind: ExternalAgentKind,
    key: &str,
) -> anyhow::Result<()> {
    let mut session = session_manager
        .get(session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
    // Read-modify-write: preserve workspace_dir (and bail loudly if
    // we're being called on a non-External session, which shouldn't
    // happen — only the External branch invokes this helper).
    let Some(baybo_model::SubagentBackendTag::External {
        external_kind: stored_kind,
        resume_key,
        ..
    }) = &mut session.state.subagent_backend
    else {
        return Err(anyhow::anyhow!(
            "session {session_id} is not an External subagent; refusing to write resume_key"
        ));
    };
    if *stored_kind != kind {
        return Err(anyhow::anyhow!(
            "session {session_id} is bound to {:?}; cannot write resume_key for {:?}",
            stored_kind.as_str(),
            kind.as_str(),
        ));
    }
    *resume_key = Some(key.to_string());
    session_manager.store().save(&session).await?;
    Ok(())
}

#[cfg(test)]
mod slot_tests {
    use super::*;
    use baybo_subagent::FanOutLimiter;
    use tokio::sync::oneshot;

    /// The fan-out slot is the child's for its whole run: released when the
    /// run ends, not when the spawn returns.
    #[tokio::test]
    async fn a_running_child_holds_its_slot_until_its_run_ends() {
        let limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter> =
            Arc::new(FanOutLimiter::new());
        let root = SessionId::from("root");
        limiter.try_reserve(&root, 8).expect("a slot is free");

        let (finish, finished) = oneshot::channel::<()>();
        let run = holding_slot(
            SubagentRun {
                child_session_id: SessionId::from("child"),
                cancel: CancellationToken::new(),
                terminal: async move {
                    let _ = finished.await;
                    SubagentResult::failed("done")
                }
                .boxed(),
            },
            Arc::clone(&limiter),
            Some(root.clone()),
        );
        let running = tokio::spawn(run.terminal);
        tokio::task::yield_now().await;
        assert_eq!(limiter.in_flight(&root), 1, "held while the child runs");

        finish.send(()).unwrap();
        running.await.unwrap();
        assert_eq!(limiter.in_flight(&root), 0, "released when the run ends");
    }
}

#[cfg(test)]
mod external_agent_cap_tests {
    use super::*;
    use baybo_model::MAX_TOOL_OUTPUT_BYTES;

    /// The external-agent leg writes to `session_messages` without passing
    /// through the agent loop's `cap_tool_output`, so a child that read a big
    /// file used to persist the whole thing as one permanent transcript row.
    #[test]
    fn oversized_tool_result_is_capped_before_persisting() {
        let mut msg = ChatMessage::tool(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "z".repeat(MAX_TOOL_OUTPUT_BYTES * 4),
            meta: None,
        }]);
        cap_external_agent_blocks(&mut msg);

        let ContentBlock::ToolResult { content, .. } = &msg.content[0] else {
            panic!("block kind must survive the cap");
        };
        assert!(
            content.len() < MAX_TOOL_OUTPUT_BYTES * 2,
            "content must be bounded, got {} bytes",
            content.len()
        );
        assert!(
            content.contains("truncated"),
            "a capped row must say so, not look complete"
        );
    }

    /// A row already within budget must be persisted byte-for-byte.
    #[test]
    fn small_message_is_untouched() {
        let mut msg = ChatMessage::assistant(vec![ContentBlock::Text("all good".into())]);
        let before = msg.clone();
        cap_external_agent_blocks(&mut msg);
        assert_eq!(msg, before);
    }
}

#[cfg(test)]
mod resume_validation_tests {
    use super::*;
    use baybo_model::{ChannelType, SessionState, SpanId, TriggerSource, TurnId, User};
    use chrono::Utc;

    fn mk_parent(id: &str) -> Session {
        Session {
            id: SessionId::from(id),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
            root_session_id: SessionId::from(id),
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    fn mk_child(
        id: &str,
        parent_id: &str,
        kind: LineageKind,
        backend_tag: Option<baybo_model::SubagentBackendTag>,
    ) -> Session {
        let mut s = mk_parent(id);
        s.lineage = Some(Lineage {
            parent_session_id: SessionId::from(parent_id),
            parent_turn_id: TurnId::default(),
            parent_span_id: Some(SpanId::default()),
            kind,
        });
        s.root_session_id = SessionId::from(parent_id);
        s.state.subagent_backend = backend_tag;
        s
    }

    fn baybo_tag() -> baybo_model::SubagentBackendTag {
        baybo_model::SubagentBackendTag::Baybo
    }

    fn mk_cron(id: &str, conversation: bool) -> Session {
        let mut s = mk_parent(id);
        s.trigger = TriggerSource::Cron {
            cron_job_id: "job-1".into(),
            origin_session_id: None,
            conversation,
            job_title: None,
            project_id: None,
        };
        s
    }

    #[test]
    fn a_session_hosts_background_jobs_only_where_a_notification_can_land() {
        // Top-level user session: the notification turn's home.
        assert!(mk_parent("p").can_host_background_jobs());

        // Subagent child: its turn ends before a notification could be
        // delivered.
        let child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        assert!(!child.can_host_background_jobs());

        // One-shot cron fire: an invisible, deliberately unregistered
        // workspace — nothing to notify into.
        assert!(!mk_cron("cr", false).can_host_background_jobs());

        // Recurring cron fire's own conversation: listed, replyable, and its
        // actor is registered, so a notification turn reaches the user. The
        // *fire's* turn is still excluded — by the turn half of the gate in
        // the agent loop, not here.
        assert!(mk_cron("cr-conv", true).can_host_background_jobs());
    }

    fn claude_tag(resume_key: Option<&str>) -> baybo_model::SubagentBackendTag {
        baybo_model::SubagentBackendTag::External {
            external_kind: ExternalAgentKind::Claude,
            workspace_dir: "test-dir".into(),
            resume_key: resume_key.map(str::to_owned),
        }
    }

    fn baybo_request() -> baybo_model::SubagentBackendKind {
        baybo_model::SubagentBackendKind::Baybo
    }

    fn claude_request() -> baybo_model::SubagentBackendKind {
        baybo_model::SubagentBackendKind::External(ExternalAgentKind::Claude)
    }

    #[test]
    fn rejects_hidden_child() {
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        child.hidden = true;
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("hidden child must reject");
        assert!(err.contains("hidden"), "got: {err}");
    }

    #[test]
    fn rejects_child_without_lineage() {
        let parent = mk_parent("p");
        let mut child = mk_parent("c");
        child.lineage = None;
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("non-subagent child must reject");
        assert!(err.contains("is not a subagent"), "got: {err}");
    }

    #[test]
    fn rejects_child_from_different_parent() {
        let parent = mk_parent("p");
        let child = mk_child(
            "c",
            "different-parent",
            LineageKind::Subagent,
            Some(baybo_tag()),
        );
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("foreign-parent child must reject");
        assert!(err.contains("different parent"), "got: {err}");
    }

    #[test]
    fn rejects_child_without_backend_tag() {
        // Pre-tag sessions (or any row that bypassed the spawn router)
        // must be refused — resume validation depends on the durable
        // tag, no inference fallback.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, None);
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("untagged child must reject");
        assert!(err.contains("no recorded subagent_backend"), "got: {err}");
    }

    #[test]
    fn baybo_rejects_external_tagged_child() {
        // Backend mismatch: child tagged External(Claude), resumed as Baybo.
        let parent = mk_parent("p");
        let child = mk_child(
            "c",
            "p",
            LineageKind::Subagent,
            Some(claude_tag(Some("uuid"))),
        );
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("backend mismatch must reject");
        assert!(err.contains("backend="), "got: {err}");
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn external_rejects_baybo_tagged_child() {
        // Reverse mismatch: child tagged Baybo, resumed as External.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        let err = validate_resume_session(&child, &parent, claude_request(), "general-purpose")
            .expect_err("baybo tag must not pass for external resume");
        assert!(err.contains("backend="), "got: {err}");
    }

    #[test]
    fn external_accepts_matching_tag_with_resume_key() {
        let parent = mk_parent("p");
        let child = mk_child(
            "c",
            "p",
            LineageKind::Subagent,
            Some(claude_tag(Some("claude-uuid"))),
        );
        validate_resume_session(&child, &parent, claude_request(), "general-purpose")
            .expect("matching tag + persisted resume_key must accept");
    }

    #[test]
    fn external_rejects_resume_without_resume_key() {
        // External child whose first call failed before emitting
        // its session-id event. No persisted resume_key. Resuming
        // would silently start a fresh conversation under the
        // existing child_session_id — refuse so the parent has to
        // spawn fresh.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(claude_tag(None)));
        let err = validate_resume_session(&child, &parent, claude_request(), "general-purpose")
            .expect_err("missing resume_key must reject");
        assert!(err.contains("no persisted"), "got: {err}");
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn baybo_accepts_fresh_baybo_child() {
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect("baybo→baybo with matching tag must accept");
    }

    #[test]
    fn rejects_resume_with_mismatched_subagent_type() {
        // A child spawned as `planner` can't be resumed as
        // `general-purpose` — that would run a different profile's
        // prompt over the existing transcript.
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        child.state.subagent_type = Some("planner".into());
        let err = validate_resume_session(&child, &parent, baybo_request(), "general-purpose")
            .expect_err("profile swap must reject");
        assert!(err.contains("planner"), "got: {err}");
        assert!(err.contains("general-purpose"), "got: {err}");
    }

    #[test]
    fn accepts_resume_with_matching_subagent_type() {
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(baybo_tag()));
        child.state.subagent_type = Some("planner".into());
        validate_resume_session(&child, &parent, baybo_request(), "planner")
            .expect("matching subagent_type must accept");
    }
}
