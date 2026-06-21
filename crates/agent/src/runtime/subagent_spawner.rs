use std::sync::Arc;
use std::time::Duration;

use aura_channels::{AgentOutput, IncomingMessage, Message};
use aura_cost::CostManager;
use aura_job::{CancelReason, JobInput, JobLifecycle, JobOutput, JobShape};
use aura_llm::TokenUsage;
use aura_model::{
    BACKGROUND_DISPATCH_ACK_PREFIX, ChannelType, ChatMessage, ContentBlock, ExternalAgentKind,
    JobId, Lineage, LineageKind, MessageMetadata, OnTimeout, PendingBackgroundResult,
    SUBAGENT_CHANNEL_TAG, Session, SessionId, SpanId, SubagentBackend, SubagentExitStatus,
    SubagentParentContext, SubagentResult, SubagentSpawnRequest, TriggerKind, User,
};
use aura_session::SessionManager;
use aura_workspace::WorkspacePaths;
use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::router::{ActorSpawner, build_oneshot_actor};
use crate::actor::subagent::await_subagent_terminal;
use crate::actor::supervisor::AgentSupervisor;
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

/// How long a foreground subagent from a user-facing session blocks the
/// parent before its `on_timeout` policy kicks in (convert to background,
/// or kill). Fixed, no per-call knob.
const SUBAGENT_FOREGROUND_WAIT: Duration = Duration::from_secs(120);

/// Whether a parent session can host a background job — delegates to the
/// shared [`aura_model::Session::supports_background_jobs`] so the router
/// (subagent conversion) and the agent loop (bash sink injection) gate on
/// one definition.
fn parent_supports_background(session: &Session) -> bool {
    session.supports_background_jobs()
}

/// Construction bundle for [`ActorSubagentSpawner`] — every field is
/// required; call sites populate it via struct literal.
pub struct SubagentSpawnerConfig {
    pub session_manager: Arc<SessionManager>,
    pub supervisor: AgentSupervisor,
    pub dispatch_limiter: Arc<dyn aura_subagent::SubagentDispatchLimiter>,
    pub cost_manager: Arc<CostManager>,
    pub job_lifecycle: Arc<JobLifecycle>,
    pub actor_parent_token: CancellationToken,
    pub external_agents: Arc<ExternalAgentRegistry>,
    pub llm_pool: LlmPoolHandle,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub actor_spawner: ActorSpawner,
}

/// Actor-backed [`aura_subagent::SubagentSpawner`]: materialises a child
/// `AgentActor` (or routes to an external backend) for each
/// `spawn_subagent` call. Lifted out of the router so the tool reaches it
/// directly — there is no cross-actor channel. The child's agent loop
/// still self-drives on its own task; `actor_spawner` just hands back a
/// mailbox.
pub struct ActorSubagentSpawner {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    dispatch_limiter: Arc<dyn aura_subagent::SubagentDispatchLimiter>,
    cost_manager: Arc<CostManager>,
    job_lifecycle: Arc<JobLifecycle>,
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
            supervisor,
            dispatch_limiter,
            cost_manager,
            job_lifecycle,
            actor_parent_token,
            external_agents,
            llm_pool,
            workspace_paths,
            actor_spawner,
        } = config;
        Self {
            session_manager,
            supervisor,
            dispatch_limiter,
            cost_manager,
            job_lifecycle,
            actor_parent_token,
            external_agents,
            llm_pool,
            workspace_paths,
            actor_spawner,
        }
    }
}

#[async_trait::async_trait]
impl aura_subagent::SubagentSpawner for ActorSubagentSpawner {
    async fn spawn(
        &self,
        parent: SubagentParentContext,
        request: SubagentSpawnRequest,
    ) -> SubagentResult {
        // The cross-actor envelope + channel are gone — `spawn` runs on the
        // tool's own task. The fan-out wait still runs on a detached task (a
        // convertible foreground child outlives this call once it converts),
        // so an internal oneshot bridges its terminal — or the immediate
        // background ack — back to this return.
        let (result_tx, result_rx) = oneshot::channel();
        if let Err(e) = self
            .handle_subagent_spawn(
                parent.session_id,
                parent.job_id,
                parent.span_id,
                parent.cancel_token,
                request,
                result_tx,
            )
            .await
        {
            return SubagentResult::failed(format!("subagent spawn dispatch error: {e}"));
        }
        result_rx.await.unwrap_or_else(|_| {
            SubagentResult::failed("subagent result channel closed before delivery")
        })
    }
}

impl ActorSubagentSpawner {
    async fn handle_subagent_spawn(
        &self,
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        // `fan_out_root` is `None` only on synthesized test requests
        // (those tests don't gate the limiter, so the release is a
        // no-op). Production spawns always carry a root because the
        // tool reserved a slot before sending the envelope.
        let fan_out_root = request.fan_out_root.clone();
        // Namespace a grouped spawn's cohort tag by the dispatching turn's
        // `job_id` (see `GroupState::cohort_key`). The agent loop counts the
        // member into the same job-scoped cohort, so reusing a group name in a
        // later turn opens a fresh cohort instead of extending a prior turn's
        // still-draining one. No-op for ungrouped spawns.
        let mut request = request;
        if let Some(group) = request.group.take() {
            request.group = Some(aura_model::GroupState::cohort_key(parent_job_id, &group));
        }
        let parent = match self.session_manager.get(&parent_session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                let _ = result_tx.send(SubagentResult::failed(format!(
                    "parent session {parent_session_id} not found"
                )));
                self.release_fan_out_slot(&fan_out_root);
                return Ok(());
            }
            Err(e) => {
                let _ = result_tx.send(SubagentResult::failed(format!("load parent session: {e}")));
                self.release_fan_out_slot(&fan_out_root);
                return Ok(());
            }
        };

        match request.backend.clone() {
            SubagentBackend::Aura => {
                self.spawn_aura_subagent(
                    parent,
                    parent_job_id,
                    parent_span_id,
                    parent_actor_token,
                    request,
                    result_tx,
                )
                .await
            }
            SubagentBackend::External { external_kind } => {
                self.spawn_external_subagent(
                    parent,
                    parent_job_id,
                    parent_span_id,
                    parent_actor_token,
                    request,
                    external_kind,
                    result_tx,
                )
                .await
            }
        }
    }

    /// In-process backend: spawn a full `AgentActor` for the child.
    /// Supports `background` fire-and-forget dispatch (result escorted
    /// back to the parent's mailbox) and resume of a prior Aura child.
    async fn spawn_aura_subagent(
        &self,
        parent: Session,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        let fan_out_root = request.fan_out_root.clone();
        let child_session = match self
            .resolve_child_session(
                &parent,
                parent_job_id,
                parent_span_id,
                &request,
                aura_model::SubagentBackendKind::Aura,
            )
            .await
        {
            Ok(s) => s,
            Err(failed) => {
                let _ = result_tx.send(failed);
                self.release_fan_out_slot(&fan_out_root);
                return Ok(());
            }
        };

        // Foreground/nested spawns subscribe to terminal events BEFORE the
        // build+dispatch so a child that exits synchronously can't slip its
        // terminal past us. Background spawns subscribe inside their detached
        // task instead — see the `if background` arm — so this receiver is
        // consumed only on the foreground paths below.
        let terminal_rx = self.job_lifecycle.subscribe_lifecycle_events();

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
        };
        let child_session_id = child_session.id.clone();
        // Tier resolution: `model_tier` lookup, falling through to the
        // pool default (handled inside the spawner closure when `None`
        // is passed). The tool already merged the profile's default_tier
        // into `model_tier`, so this is the final step.
        let llm = request
            .model_tier
            .and_then(|t| self.llm_pool.read().resolve_tier(t));
        let on_timeout = request.on_timeout;
        let subagent_type = request.subagent_type.clone();
        let task_summary = request.task_summary.clone();
        // Barrier cohort (background-from-start spawns only). Tagged onto the
        // escorted result so the parent holds it until the group completes.
        let group = request.group.clone();

        // A foreground subagent from a live user session converts to
        // background after a fixed foreground wait (unless `on_timeout`
        // is `Kill`). Cron and nested-subagent parents are out of scope —
        // their notification turn can't be delivered — so they keep the
        // block-until-terminal behaviour with no timer.
        let user_facing = parent_supports_background(&parent);
        // A non-user parent can't host the notification turn that surfaces a
        // background result, so an explicit `background=true` / grouped spawn
        // from one is downgraded to a blocking foreground run (its result is
        // returned inline) rather than dispatched to a notification that would
        // be dropped on delivery. `convertible` is already user-only.
        let background = request.background && user_facing;
        let convertible = user_facing && !background && matches!(on_timeout, OnTimeout::Background);

        // Background subagents — and convertible foreground ones, which
        // may outlive the dispatching turn once they convert — must
        // outlive the parent's per-job cancel scope: the job that emitted
        // `spawn_subagent` ends as soon as the tool returns, so anchoring
        // the child to that token would tear it down immediately. The
        // process-wide `actor_parent_token` is the right ancestor —
        // process shutdown still cascades. A `Kill`-on-timeout foreground
        // spawn stays on the parent token (it is cancelled at the
        // foreground-wait mark anyway).
        let effective_parent_token = if background || convertible {
            self.actor_parent_token.clone()
        } else {
            parent_actor_token.clone()
        };

        let (output_tx, output_rx) = mpsc::channel::<AgentOutput>(SUBAGENT_OUTPUT_BUFFER);
        let (mailbox, actor_token) = build_oneshot_actor(
            &self.actor_spawner,
            &effective_parent_token,
            child_session,
            llm,
            output_tx,
        );

        // The prompt (`SubagentSpawned`) kicks the parked child actor into
        // real work. A background spawn feeds it on a detached escort task so
        // the child outlives the dispatching turn; foreground/nested paths feed
        // it inline below and block on the terminal.
        if background {
            let handle_id = self
                .ack_background_dispatch(
                    &parent.id,
                    &child_session_id,
                    &subagent_type,
                    &task_summary,
                    actor_token.clone(),
                    result_tx,
                )
                .await;
            let job_lifecycle = Arc::clone(&self.job_lifecycle);
            let supervisor = self.supervisor.clone();
            let parent_id_for_task = parent.id.clone();
            let fan_out_root_for_task = fan_out_root.clone();
            let limiter_for_task = Arc::clone(&self.dispatch_limiter);
            tokio::spawn(async move {
                // Subscribe before feeding the prompt so a child that exits
                // quickly can't slip its terminal past us.
                let terminal_rx = job_lifecycle.subscribe_lifecycle_events();
                let result = if let Err(e) = mailbox
                    .send(AgentMessage::SubagentSpawned {
                        initial_message: Box::new(incoming),
                        parent_job_id,
                    })
                    .await
                {
                    SubagentResult::failed(format!("dispatch child input: {e}"))
                } else {
                    await_subagent_terminal(
                        child_session_id.clone(),
                        output_rx,
                        terminal_rx,
                        mailbox,
                        actor_token,
                        job_lifecycle,
                    )
                    .await
                };
                escort_background_terminal(
                    &supervisor,
                    &parent_id_for_task,
                    handle_id,
                    subagent_type,
                    task_summary,
                    group,
                    result,
                    &limiter_for_task,
                    &fan_out_root_for_task,
                )
                .await;
            });
            return Ok(());
        }

        // Foreground / nested: feed the prompt now (parked actor → running),
        // then wait on the terminal.
        if let Err(e) = mailbox
            .send(AgentMessage::SubagentSpawned {
                initial_message: Box::new(incoming),
                parent_job_id,
            })
            .await
        {
            let _ = result_tx.send(SubagentResult::failed(format!("dispatch child input: {e}")));
            self.release_fan_out_slot(&fan_out_root);
            return Ok(());
        }

        // Foreground dispatch under the shared wait/convert/kill policy. The
        // terminal future is the Aura actor's terminal observer; the policy
        // (block for nested, or wait-then-convert/kill for a user parent) is
        // backend-agnostic and lives in `run_foreground_job`.
        let fut = await_subagent_terminal(
            child_session_id.clone(),
            output_rx,
            terminal_rx,
            mailbox,
            actor_token.clone(),
            Arc::clone(&self.job_lifecycle),
        );
        tokio::spawn(run_foreground_job(
            ForegroundJob {
                user_facing,
                on_timeout,
                child_session_id,
                subagent_type,
                task_summary,
                child_token: actor_token,
                parent_cancel: parent_actor_token.clone(),
                result_tx,
                fan_out_root,
            },
            fut,
            self.supervisor.clone(),
            Arc::clone(&self.session_manager),
            parent.id.clone(),
            Arc::clone(&self.dispatch_limiter),
        ));
        Ok(())
    }

    /// External backend: route one-shot delegation to a registered
    /// external-agent impl (claude_cli, codex_cli). No `AgentActor` is
    /// built; the agent's event stream is driven to a terminal result.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_external_subagent(
        &self,
        parent: Session,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        kind: ExternalAgentKind,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        let fan_out_root = request.fan_out_root.clone();
        let Some(agent) = self.external_agents.get(kind) else {
            let _ = result_tx.send(SubagentResult::failed(format!(
                "external agent {:?} is not registered (check `aura external-agent` config + boot logs)",
                kind.as_str(),
            )));
            self.release_fan_out_slot(&fan_out_root);
            return Ok(());
        };

        let child_session = match self
            .resolve_child_session(
                &parent,
                parent_job_id,
                parent_span_id,
                &request,
                aura_model::SubagentBackendKind::External(kind),
            )
            .await
        {
            Ok(s) => s,
            Err(failed) => {
                let _ = result_tx.send(failed);
                self.release_fan_out_slot(&fan_out_root);
                return Ok(());
            }
        };
        let (dir_name, resume_key) = match &child_session.state.subagent_backend {
            Some(aura_model::SubagentBackendTag::External {
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

        // Background — and convertible-foreground — external runs anchor to the
        // process-wide token so they outlive the dispatching turn (claude/codex
        // runs are long; a converted one keeps running past the turn that
        // spawned it). A non-convertible foreground run stays on the parent's
        // per-job token (cancelled at the foreground-wait mark, or ends with the
        // turn). Mirrors the Aura backend's anchoring.
        let user_facing = parent_supports_background(&parent);
        // Non-user parents can't deliver a background notification, so an
        // explicit background / grouped external run from one is downgraded to
        // a blocking foreground run (result returned inline). Mirrors the Aura
        // backend's gate.
        let background = request.background && user_facing;
        let convertible =
            user_facing && !background && matches!(request.on_timeout, OnTimeout::Background);
        let actor_token = if background || convertible {
            self.actor_parent_token.child_token()
        } else {
            parent_actor_token.child_token()
        };
        let child_session_id = child_session.id.clone();
        let session_manager = Arc::clone(&self.session_manager);
        let job_ctx = ExternalJobCtx {
            lifecycle: Arc::clone(&self.job_lifecycle),
            cost_manager: Arc::clone(&self.cost_manager),
            user_id: child_session.user.id.clone(),
            trigger_kind: child_session.trigger.kind(),
            parent_job_id,
        };
        let limiter_for_task = Arc::clone(&self.dispatch_limiter);
        let external_request = ExternalAgentRequest {
            task: request.prompt.clone(),
            workspace_dir,
            resume_key,
            cancel: actor_token,
            timeout: EXTERNAL_SUBAGENT_TIMEOUT,
        };

        if background {
            let handle_id = self
                .ack_background_dispatch(
                    &parent.id,
                    &child_session_id,
                    &request.subagent_type,
                    &request.task_summary,
                    external_request.cancel.clone(),
                    result_tx,
                )
                .await;
            let supervisor = self.supervisor.clone();
            let parent_id_for_task = parent.id.clone();
            let subagent_type = request.subagent_type.clone();
            let task_summary = request.task_summary.clone();
            let group = request.group.clone();
            let fan_out_root_for_task = fan_out_root.clone();
            tokio::spawn(async move {
                let result = run_external_agent_job(
                    agent,
                    kind,
                    external_request,
                    child_session_id.clone(),
                    session_manager,
                    job_ctx,
                )
                .await;
                escort_background_terminal(
                    &supervisor,
                    &parent_id_for_task,
                    handle_id,
                    subagent_type,
                    task_summary,
                    group,
                    result,
                    &limiter_for_task,
                    &fan_out_root_for_task,
                )
                .await;
            });
            return Ok(());
        }

        // Foreground external run under the shared wait/convert/kill policy: a
        // user parent gets the same foreground-wait → convert-to-background
        // behaviour as the Aura backend; a nested/cron parent blocks until the
        // run finishes (or hits `EXTERNAL_SUBAGENT_TIMEOUT`). The cancel token
        // is the foreground job's `child_token` so a `/stop` or `Kill`-timeout
        // reaches the in-flight subprocess.
        let child_token = external_request.cancel.clone();
        let fut = run_external_agent_job(
            agent,
            kind,
            external_request,
            child_session_id.clone(),
            Arc::clone(&session_manager),
            job_ctx,
        );
        tokio::spawn(run_foreground_job(
            ForegroundJob {
                user_facing,
                on_timeout: request.on_timeout,
                child_session_id,
                subagent_type: request.subagent_type.clone(),
                task_summary: request.task_summary.clone(),
                child_token,
                parent_cancel: parent_actor_token.clone(),
                result_tx,
                fan_out_root,
            },
            fut,
            self.supervisor.clone(),
            session_manager,
            parent.id.clone(),
            limiter_for_task,
        ));
        Ok(())
    }

    fn release_fan_out_slot(&self, root: &Option<SessionId>) {
        release_reserved_slot(self.dispatch_limiter.as_ref(), root);
    }

    /// Send the immediate "[background subagent dispatched]" ack to the
    /// parent's tool boundary and pin the parent against the idle reaper
    /// for the lifetime of the background child. Returns the synthetic
    /// handle id stamped on the ack (and later on the escorted result).
    /// Shared by the Aura and External background paths.
    async fn ack_background_dispatch(
        &self,
        parent_id: &SessionId,
        child_session_id: &SessionId,
        subagent_type: &str,
        task_summary: &str,
        cancel_token: CancellationToken,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> String {
        let handle_id = aura_model::new_background_handle();
        let ack_text = format!(
            "{BACKGROUND_DISPATCH_ACK_PREFIX}\n- handle: {handle_id}\n- subagent_type: {subagent_type}\n- child_session: {child_session_id}\n\nThe runtime will surface the subagent's final message as a system reminder prepended to your next user turn."
        );
        let _ = result_tx.send(SubagentResult {
            child_session_id: child_session_id.clone(),
            final_content: Some(vec![ContentBlock::Text(ack_text)]),
            status: SubagentExitStatus::Completed,
        });
        self.supervisor.note_background_subagent_started(
            parent_id,
            child_session_id,
            subagent_type,
            task_summary,
            &handle_id,
            cancel_token,
        );
        if let Err(e) = self.session_manager.touch(parent_id).await {
            warn!(
                parent_session_id = %parent_id,
                error = %e,
                "background spawn: failed to touch parent session"
            );
        }
        handle_id
    }

    /// Returns the child Session — loading an existing one when
    /// `resume_session_id` is set, otherwise minting a new
    /// `LineageKind::Subagent` row. Backend-mismatch + parent +
    /// hidden + lineage checks all run on the resume path.
    async fn resolve_child_session(
        &self,
        parent: &Session,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        request: &SubagentSpawnRequest,
        backend: aura_model::SubagentBackendKind,
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
                parent_job_id,
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
                aura_model::SubagentBackendKind::Aura => aura_model::SubagentBackendTag::Aura,
                aura_model::SubagentBackendKind::External(kind) => {
                    aura_model::SubagentBackendTag::External {
                        external_kind: kind,
                        workspace_dir: child.id.as_ref().to_string(),
                        resume_key: None,
                    }
                }
            });
            child.state.subagent_type = Some(request.subagent_type.clone());
            if let Err(e) = self.session_manager.store().save(&child).await {
                return Err(SubagentResult::failed(format!(
                    "persist subagent identity on {}: {e}",
                    child.id,
                )));
            }
            Ok(child)
        }
    }
}

/// Release a reserved fan-out slot for `root`, if any. A free fn so the
/// detached wait tasks — which capture only an
/// `Arc<dyn SubagentDispatchLimiter>`, not `&self` — share the `&self`
/// method's single implementation.
fn release_reserved_slot(
    limiter: &dyn aura_subagent::SubagentDispatchLimiter,
    root: &Option<SessionId>,
) {
    if let Some(id) = root {
        limiter.release(id);
    }
}

/// Terminal tail shared by both background backends: escort the result
/// to the parent's mailbox, THEN clear the reaper counter and release
/// the fan-out slot. The clear must happen AFTER the escort so the
/// reaper can't tear the parent down in the window between
/// terminal-observe and delivery.
#[allow(clippy::too_many_arguments)]
async fn escort_background_terminal(
    supervisor: &AgentSupervisor,
    parent_id: &SessionId,
    handle_id: String,
    subagent_type: String,
    task_summary: String,
    group: Option<String>,
    result: SubagentResult,
    limiter: &Arc<dyn aura_subagent::SubagentDispatchLimiter>,
    fan_out_root: &Option<SessionId>,
) {
    let child_session_id = result.child_session_id.clone();
    // Peek (don't clear) so the in-flight marker is still set across the
    // delivery — the reaper must not tear the parent down mid-escort. An
    // absent marker means `/stop` already drained this subagent, so suppress
    // the terminal delivery: a user-stopped result must not repopulate
    // `pending_background_results`.
    if supervisor.is_background_subagent_in_flight(parent_id, &child_session_id) {
        deliver_background_result(
            supervisor,
            parent_id,
            handle_id,
            subagent_type,
            task_summary,
            group,
            result,
        )
        .await;
    } else {
        debug!(
            parent_session_id = %parent_id,
            child_session_id = %child_session_id,
            "background subagent was /stop-cancelled; suppressing terminal delivery"
        );
    }
    supervisor.note_background_subagent_finished(parent_id, &child_session_id);
    release_reserved_slot(limiter.as_ref(), fan_out_root);
}

/// Everything [`run_foreground_job`] needs that isn't the terminal future.
struct ForegroundJob {
    /// Whether the parent can host the background-conversion notification
    /// turn ([`Session::supports_background_jobs`]). A non-user parent blocks
    /// until terminal with no foreground-wait timer.
    user_facing: bool,
    on_timeout: OnTimeout,
    child_session_id: SessionId,
    subagent_type: String,
    task_summary: String,
    /// Cancels the underlying work — the child actor for the Aura backend, the
    /// external subprocess for the External one. Registered on conversion so
    /// `/stop` reaches the now-background job, and fired directly on `Kill`.
    child_token: CancellationToken,
    /// The parent turn's cancel scope. A convertible child is anchored to the
    /// process-wide token (so it survives the dispatching turn once it
    /// converts), so the parent's `/stop` does NOT cascade to it during the
    /// foreground window. `run_foreground_job` watches this to cancel a
    /// still-foreground child when the parent turn is stopped, so a stopped
    /// turn can't leave a subagent running on to convert and notify.
    parent_cancel: CancellationToken,
    result_tx: oneshot::Sender<SubagentResult>,
    fan_out_root: Option<SessionId>,
}

/// Run a foreground subagent's terminal `fut` under the foreground-wait
/// policy — the half of foreground dispatch that's identical across backends
/// (only `fut` differs: an Aura actor terminal vs an external-subprocess run,
/// both resolving to a [`SubagentResult`]).
///
/// A non-user parent blocks until terminal (no timer). A user-facing parent
/// waits up to [`SUBAGENT_FOREGROUND_WAIT`]; on overrun it either converts the
/// still-running job to background (acking now, escorting its eventual
/// terminal as a notification turn) or force-cancels it, per `on_timeout`. The
/// future is pinned and resumed across the `select!` boundary so it is never
/// polled to completion twice.
async fn run_foreground_job(
    job: ForegroundJob,
    fut: impl std::future::Future<Output = SubagentResult>,
    supervisor: AgentSupervisor,
    session_manager: Arc<aura_session::SessionManager>,
    parent_id: SessionId,
    limiter: Arc<dyn aura_subagent::SubagentDispatchLimiter>,
) {
    let ForegroundJob {
        user_facing,
        on_timeout,
        child_session_id,
        subagent_type,
        task_summary,
        child_token,
        parent_cancel,
        result_tx,
        fan_out_root,
    } = job;
    tokio::pin!(fut);

    if !user_facing {
        // Cron / nested-subagent parent: conversion + its notification turn
        // only work for a live, registered user session, so block until
        // terminal with no foreground-wait timer.
        let result = fut.await;
        let _ = result_tx.send(result);
        release_reserved_slot(limiter.as_ref(), &fan_out_root);
        return;
    }

    // A convertible child runs on the process-wide token, so the parent's
    // `/stop` won't cascade to it — watch the parent's cancel scope here so a
    // stopped turn tears the still-foreground child down instead of letting it
    // convert and notify later.
    let stop_token = child_token.clone();
    tokio::select! {
        result = &mut fut => {
            // Finished within the foreground window — normal result.
            let _ = result_tx.send(result);
            release_reserved_slot(limiter.as_ref(), &fan_out_root);
        }
        _ = parent_cancel.cancelled() => {
            // `/stop` during the foreground window: cancel the child, let it
            // observe the cancel and drain, then surface a cancelled result.
            stop_token.cancel();
            let _ = fut.await;
            let _ = result_tx.send(SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Cancelled,
            });
            release_reserved_slot(limiter.as_ref(), &fan_out_root);
        }
        _ = tokio::time::sleep(SUBAGENT_FOREGROUND_WAIT) => match on_timeout {
            OnTimeout::Background => {
                let handle_id = convert_foreground_to_background(
                    &supervisor,
                    &session_manager,
                    &parent_id,
                    &child_session_id,
                    &subagent_type,
                    &task_summary,
                    child_token,
                    result_tx,
                )
                .await;
                let result = fut.await;
                escort_background_terminal(
                    &supervisor,
                    &parent_id,
                    handle_id,
                    subagent_type,
                    task_summary,
                    // A converted foreground subagent is never grouped
                    // (grouped spawns are background-from-start).
                    None,
                    result,
                    &limiter,
                    &fan_out_root,
                )
                .await;
            }
            OnTimeout::Kill => {
                // Force-cancel the underlying work, let it observe the cancel
                // and drain, then surface a timeout on the foreground oneshot.
                child_token.cancel();
                let _ = fut.await;
                let _ = result_tx.send(SubagentResult {
                    child_session_id,
                    final_content: None,
                    status: SubagentExitStatus::Timeout,
                });
                release_reserved_slot(limiter.as_ref(), &fan_out_root);
            }
        }
    }
}

/// Convert a still-running foreground subagent into a background one once
/// it exceeds the foreground wait: ack the conversion on the foreground
/// oneshot (so the parent's tool call returns now), register it with the
/// supervisor (pinning the parent against the idle reaper), and return the
/// handle the caller uses to escort the eventual terminal. Mirrors
/// [`Router::ack_background_dispatch`] but runs from the detached wait
/// task, so it takes cloned `supervisor` / `session_manager` handles
/// instead of `&self`.
#[allow(clippy::too_many_arguments)]
async fn convert_foreground_to_background(
    supervisor: &AgentSupervisor,
    session_manager: &aura_session::SessionManager,
    parent_id: &SessionId,
    child_session_id: &SessionId,
    subagent_type: &str,
    task_summary: &str,
    child_token: CancellationToken,
    result_tx: oneshot::Sender<SubagentResult>,
) -> String {
    let handle_id = aura_model::new_background_handle();
    let ack_text = format!(
        "[foreground subagent exceeded its {}s foreground wait — converted to background]\n- handle: {handle_id}\n- subagent_type: {subagent_type}\n- child_session: {child_session_id}\n\nIt keeps running; the runtime will surface its final message as a system reminder on a later turn.",
        SUBAGENT_FOREGROUND_WAIT.as_secs()
    );
    let _ = result_tx.send(SubagentResult {
        // Empty child id: this ack must NOT carry a resume tail — the child is
        // still running in the background, so the parent can't resume it. The
        // foreground tool path appends `[subagent_session_id: …]` only for a
        // Completed result with a non-empty id; `ack_text` names the child
        // session for reference, and the escorted terminal surfaces the real
        // resume id later.
        child_session_id: SessionId::from(""),
        final_content: Some(vec![ContentBlock::Text(ack_text)]),
        status: SubagentExitStatus::Completed,
    });
    supervisor.note_background_subagent_started(
        parent_id,
        child_session_id,
        subagent_type,
        task_summary,
        &handle_id,
        child_token,
    );
    if let Err(e) = session_manager.touch(parent_id).await {
        warn!(
            parent_session_id = %parent_id,
            error = %e,
            "convert-to-background: failed to touch parent session"
        );
    }
    handle_id
}

fn validate_resume_session(
    child: &Session,
    parent: &Session,
    backend: aura_model::SubagentBackendKind,
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
    if let aura_model::SubagentBackendTag::External {
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

/// Job-lifecycle metadata for an external subagent run. The child
/// session needs its own `Spawned` job for the trace browser to
/// surface it — `list_session_summaries` drops zero-job sessions and
/// `get_trace` 404s on them.
struct ExternalJobCtx {
    lifecycle: Arc<JobLifecycle>,
    cost_manager: Arc<CostManager>,
    user_id: String,
    trigger_kind: TriggerKind,
    parent_job_id: JobId,
}

/// Wrap [`run_external_agent`] in a `Spawned` job so the external
/// subagent's child session is visible/inspectable in the trace
/// browser, mirroring how the in-process Aura backend's actor creates
/// a job per turn. The terminal `SubagentExitStatus` maps onto the
/// job's terminal transition (Completed→complete, Failed→fail,
/// Timeout→cancel(SubagentTimeout), Cancelled→cancel(ParentCancelled)).
///
/// Uses the `JobLifecycle` primitives directly rather than
/// `scope::with_job` because `with_job` collapses every non-success
/// exit into `fail()`; the subagent contract needs the distinct
/// `SubagentTimeout` / `ParentCancelled` cancel reasons preserved.
/// Terminal-race transitions (operator cancel landing as the run
/// completes) surface as `InvalidTransition`, swallowed via `let _`.
async fn run_external_agent_job(
    agent: Arc<dyn ExternalAgent>,
    kind: ExternalAgentKind,
    request: ExternalAgentRequest,
    child_session_id: SessionId,
    session_manager: Arc<aura_session::SessionManager>,
    job_ctx: ExternalJobCtx,
) -> SubagentResult {
    let cancel = request.cancel.clone();
    let initial_prompt = vec![ContentBlock::Text(request.task.clone())];
    let job = match job_ctx
        .lifecycle
        .start_job(
            child_session_id.clone(),
            job_ctx.trigger_kind,
            JobShape::Turn,
            JobInput::Spawned { initial_prompt },
            Some(job_ctx.parent_job_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Failed {
                    reason: format!("start subagent job: {e}"),
                },
            };
        }
    };
    // Register the cancel token so an operator-issued job cancel trips
    // the run, then move Pending → InProgress.
    let _cancel_guard = job_ctx.lifecycle.register_running(job.id, cancel);
    if let Err(e) = job_ctx.lifecycle.start(&job.id).await {
        let _ = job_ctx.lifecycle.fail(&job.id, format!("start: {e}")).await;
        return SubagentResult {
            child_session_id,
            final_content: None,
            status: SubagentExitStatus::Failed {
                reason: format!("start subagent job: {e}"),
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
        job_ctx.cost_manager.record_external_tokens(
            &job_ctx.user_id,
            job.session_id.clone(),
            job.id,
            SpanId::default(),
            aura_llm::CallReason::Chat,
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
            let _ = job_ctx
                .lifecycle
                .complete(&job.id, JobOutput::Message { content })
                .await;
        }
        SubagentExitStatus::Failed { reason } => {
            let _ = job_ctx.lifecycle.fail(&job.id, reason.clone()).await;
        }
        SubagentExitStatus::Timeout => {
            let _ = job_ctx
                .lifecycle
                .cancel(&job.id, CancelReason::SubagentTimeout, vec![])
                .await;
        }
        SubagentExitStatus::Cancelled => {
            let _ = job_ctx
                .lifecycle
                .cancel(&job.id, CancelReason::ParentCancelled, vec![])
                .await;
        }
    }

    result
}

/// Drives the agent stream → persists transcript turns, captures the
/// terminal status, and returns any `Usage` the agent reported (for
/// the caller to log into `cost_records`). Stays job-agnostic;
/// [`run_external_agent_job`] owns the surrounding job lifecycle.
async fn run_external_agent(
    agent: Arc<dyn ExternalAgent>,
    kind: ExternalAgentKind,
    request: ExternalAgentRequest,
    child_session_id: SessionId,
    session_manager: Arc<aura_session::SessionManager>,
) -> (SubagentResult, Option<TokenUsage>) {
    // Persist the operator-supplied task up-front so the transcript
    // shows something even if the external agent crashes before
    // emitting its first event.
    append_subagent_message(
        &session_manager,
        &child_session_id,
        // The task is the parent agent's instruction to the subagent, not a
        // human channel input — agent-context, so it never renders as a user
        // bubble in the child session's transcript.
        ChatMessage::agent_context(vec![ContentBlock::Text(request.task.clone())]),
    )
    .await;

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
                // run_external_agent_job after the stream closes.
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
                append_subagent_message(&session_manager, &child_session_id, msg).await;
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
    session_manager: &Arc<aura_session::SessionManager>,
    session_id: &SessionId,
    msg: ChatMessage,
) {
    if let Err(e) = session_manager
        .append_session_message(session_id, &msg)
        .await
    {
        warn!(
            session_id = %session_id,
            role = ?msg.role,
            error = %e,
            "failed to persist external-agent turn to session_messages",
        );
    }
}

async fn persist_resume_key(
    session_manager: &Arc<aura_session::SessionManager>,
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
    let Some(aura_model::SubagentBackendTag::External {
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

/// Post the background subagent's terminal result to the parent
/// actor's mailbox so the next user turn picks it up as a system
/// reminder.
///
/// If the parent actor is no longer registered (idle-reaped between
/// spawn and finish, or never rehydrated after a crash), `route`
/// returns false. We log a warning; the result is still preserved in
/// the trace tree and the child's session row. On the actor side the
/// pending result is persisted on receipt, so a parent reaped AFTER
/// delivery still surfaces it on the next hydration.
#[allow(clippy::too_many_arguments)]
async fn deliver_background_result(
    supervisor: &AgentSupervisor,
    parent_session_id: &SessionId,
    handle_id: String,
    subagent_type: String,
    task_summary: String,
    group: Option<String>,
    result: SubagentResult,
) {
    let final_text = result.result_text();
    let mut pending = PendingBackgroundResult::subagent(
        handle_id.clone(),
        subagent_type,
        task_summary,
        result.child_session_id,
        final_text,
        result.status,
    );
    pending.group = group;
    let delivered = supervisor
        .route(
            parent_session_id,
            AgentMessage::BackgroundJobFinished(Box::new(pending)),
        )
        .await;
    if !delivered {
        warn!(
            parent_session_id = %parent_session_id,
            handle_id = %handle_id,
            "background subagent terminal could not be routed — parent actor not registered; result available in trace/child session only"
        );
    }
}

#[cfg(test)]
mod resume_validation_tests {
    use super::*;
    use aura_model::{ChannelType, JobId, SessionState, SpanId, TriggerSource, User};
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
            folder_id: None,
        }
    }

    fn mk_child(
        id: &str,
        parent_id: &str,
        kind: LineageKind,
        backend_tag: Option<aura_model::SubagentBackendTag>,
    ) -> Session {
        let mut s = mk_parent(id);
        s.lineage = Some(Lineage {
            parent_session_id: SessionId::from(parent_id),
            parent_job_id: JobId::default(),
            parent_span_id: Some(SpanId::default()),
            kind,
        });
        s.root_session_id = SessionId::from(parent_id);
        s.state.subagent_backend = backend_tag;
        s
    }

    fn aura_tag() -> aura_model::SubagentBackendTag {
        aura_model::SubagentBackendTag::Aura
    }

    #[test]
    fn parent_supports_background_only_for_top_level_user_sessions() {
        // Top-level user session: convertible.
        assert!(parent_supports_background(&mk_parent("p")));

        // Subagent child: not convertible (its turn ends before a
        // notification could be delivered).
        let child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        assert!(!parent_supports_background(&child));

        // Cron session: one-shot + unregistered, so notification can't
        // reach it.
        let mut cron = mk_parent("cr");
        cron.trigger = TriggerSource::Cron {
            cron_job_id: "job-1".into(),
        };
        assert!(!parent_supports_background(&cron));
    }

    fn claude_tag(resume_key: Option<&str>) -> aura_model::SubagentBackendTag {
        aura_model::SubagentBackendTag::External {
            external_kind: ExternalAgentKind::Claude,
            workspace_dir: "test-dir".into(),
            resume_key: resume_key.map(str::to_owned),
        }
    }

    fn aura_request() -> aura_model::SubagentBackendKind {
        aura_model::SubagentBackendKind::Aura
    }

    fn claude_request() -> aura_model::SubagentBackendKind {
        aura_model::SubagentBackendKind::External(ExternalAgentKind::Claude)
    }

    #[test]
    fn rejects_hidden_child() {
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        child.hidden = true;
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
            .expect_err("hidden child must reject");
        assert!(err.contains("hidden"), "got: {err}");
    }

    #[test]
    fn rejects_child_without_lineage() {
        let parent = mk_parent("p");
        let mut child = mk_parent("c");
        child.lineage = None;
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
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
            Some(aura_tag()),
        );
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
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
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
            .expect_err("untagged child must reject");
        assert!(err.contains("no recorded subagent_backend"), "got: {err}");
    }

    #[test]
    fn aura_rejects_external_tagged_child() {
        // Backend mismatch: child tagged External(Claude), resumed as Aura.
        let parent = mk_parent("p");
        let child = mk_child(
            "c",
            "p",
            LineageKind::Subagent,
            Some(claude_tag(Some("uuid"))),
        );
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
            .expect_err("backend mismatch must reject");
        assert!(err.contains("backend="), "got: {err}");
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn external_rejects_aura_tagged_child() {
        // Reverse mismatch: child tagged Aura, resumed as External.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        let err = validate_resume_session(&child, &parent, claude_request(), "general-purpose")
            .expect_err("aura tag must not pass for external resume");
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
    fn aura_accepts_fresh_aura_child() {
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        validate_resume_session(&child, &parent, aura_request(), "general-purpose")
            .expect("aura→aura with matching tag must accept");
    }

    #[test]
    fn rejects_resume_with_mismatched_subagent_type() {
        // A child spawned as `planner` can't be resumed as
        // `general-purpose` — that would run a different profile's
        // prompt over the existing transcript.
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        child.state.subagent_type = Some("planner".into());
        let err = validate_resume_session(&child, &parent, aura_request(), "general-purpose")
            .expect_err("profile swap must reject");
        assert!(err.contains("planner"), "got: {err}");
        assert!(err.contains("general-purpose"), "got: {err}");
    }

    #[test]
    fn accepts_resume_with_matching_subagent_type() {
        let parent = mk_parent("p");
        let mut child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        child.state.subagent_type = Some("planner".into());
        validate_resume_session(&child, &parent, aura_request(), "planner")
            .expect("matching subagent_type must accept");
    }
}

#[cfg(test)]
mod foreground_job_tests {
    //! Backend-agnostic foreground-wait policy ([`run_foreground_job`]), shared
    //! by the Aura and External backends. The terminal future is faked here;
    //! the convert/escort plumbing it calls is exercised end-to-end by the
    //! integration suite.
    use super::*;
    use aura_session::test_support::{
        MemorySessionFolderStore, MemorySessionStore, MemorySessionSummaryStore,
    };

    fn test_supervisor() -> AgentSupervisor {
        let (tx, _rx) = mpsc::channel::<AgentOutput>(8);
        AgentSupervisor::new(tx)
    }

    fn test_session_manager() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionSummaryStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ))
    }

    fn completed(text: &str) -> SubagentResult {
        SubagentResult {
            child_session_id: SessionId::from("child"),
            final_content: Some(vec![ContentBlock::Text(text.to_string())]),
            status: SubagentExitStatus::Completed,
        }
    }

    fn job(
        user_facing: bool,
        on_timeout: OnTimeout,
        child_token: CancellationToken,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> ForegroundJob {
        ForegroundJob {
            user_facing,
            on_timeout,
            child_session_id: SessionId::from("child"),
            subagent_type: "general-purpose".into(),
            task_summary: "t".into(),
            child_token,
            // Never-cancelled by default; the parent-stop test overrides it.
            parent_cancel: CancellationToken::new(),
            result_tx,
            fan_out_root: None,
        }
    }

    fn spawn_job(
        j: ForegroundJob,
        fut: impl std::future::Future<Output = SubagentResult> + Send + 'static,
    ) {
        tokio::spawn(run_foreground_job(
            j,
            fut,
            test_supervisor(),
            test_session_manager(),
            SessionId::from("parent"),
            aura_subagent::unbounded_limiter(),
        ));
    }

    // Finishes inside the foreground window → the result passes straight
    // through, no conversion.
    #[tokio::test(start_paused = true)]
    async fn passes_through_when_fut_completes_in_time() {
        let (tx, rx) = oneshot::channel();
        spawn_job(
            job(true, OnTimeout::Background, CancellationToken::new(), tx),
            async { completed("the answer") },
        );
        let result = rx.await.expect("result");
        assert!(matches!(result.status, SubagentExitStatus::Completed));
        assert_eq!(result.result_text(), "the answer");
    }

    // User parent + `Background` + a run that overruns the foreground wait →
    // the foreground oneshot gets the tail-free conversion ack (the child keeps
    // running in the background).
    #[tokio::test(start_paused = true)]
    async fn converts_to_background_on_overrun() {
        let (tx, rx) = oneshot::channel();
        // Never resolves on its own → forces the foreground-wait timeout, which
        // paused time auto-advances to.
        spawn_job(
            job(true, OnTimeout::Background, CancellationToken::new(), tx),
            std::future::pending::<SubagentResult>(),
        );
        let ack = rx.await.expect("conversion ack");
        assert!(
            ack.result_text().contains("converted to background"),
            "got: {}",
            ack.result_text()
        );
        // Empty child id → no resume tail on the ack (the child is still
        // running, so the parent can't resume it).
        assert!(ack.resume_tail().is_none());
    }

    // User parent + `Kill` + a run that overruns → the run's token is cancelled
    // and a `Timeout` surfaces on the foreground oneshot.
    #[tokio::test(start_paused = true)]
    async fn kills_on_overrun_when_policy_is_kill() {
        let (tx, rx) = oneshot::channel();
        let token = CancellationToken::new();
        let observe = token.clone();
        // Resolves only once cancelled — mirrors a real run observing the kill.
        let fut = async move {
            observe.cancelled().await;
            SubagentResult {
                child_session_id: SessionId::from("child"),
                final_content: None,
                status: SubagentExitStatus::Cancelled,
            }
        };
        spawn_job(job(true, OnTimeout::Kill, token.clone(), tx), fut);
        let result = rx.await.expect("timeout result");
        assert!(matches!(result.status, SubagentExitStatus::Timeout));
        assert!(token.is_cancelled(), "the kill cancelled the run's token");
    }

    // A non-user parent (cron / nested subagent) has no foreground-wait timer:
    // it blocks until terminal and never converts, even on a long run.
    #[tokio::test(start_paused = true)]
    async fn non_user_parent_blocks_without_conversion() {
        let (tx, mut rx) = oneshot::channel();
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = Arc::clone(&gate);
        let fut = async move {
            gate2.notified().await;
            completed("nested done")
        };
        spawn_job(
            job(false, OnTimeout::Background, CancellationToken::new(), tx),
            fut,
        );
        // Well past the foreground wait, still no conversion ack (no timer).
        tokio::time::advance(SUBAGENT_FOREGROUND_WAIT * 3).await;
        assert!(rx.try_recv().is_err(), "a non-user parent must not convert");
        // It delivers only when the run actually finishes.
        gate.notify_one();
        let result = rx.await.expect("result after the run finishes");
        assert_eq!(result.result_text(), "nested done");
    }

    // `/stop` during the foreground window must cancel a convertible child even
    // though that child is anchored to the process-wide token (not the parent's
    // per-call scope), so a stopped turn can't leave it running on to convert
    // and notify. Without the parent-stop watch this would convert at the 2-min
    // mark and surface a Completed conversion ack instead.
    #[tokio::test(start_paused = true)]
    async fn parent_stop_during_window_cancels_the_child() {
        let (tx, rx) = oneshot::channel();
        let child_token = CancellationToken::new();
        let observe = child_token.clone();
        let check = child_token.clone();
        let mut j = job(true, OnTimeout::Background, child_token, tx);
        let parent_cancel = CancellationToken::new();
        j.parent_cancel = parent_cancel.clone();
        // The terminal resolves only once the child is cancelled — a real
        // convertible child is not reached by the parent's own cancel scope.
        spawn_job(j, async move {
            observe.cancelled().await;
            SubagentResult {
                child_session_id: SessionId::from("child"),
                final_content: None,
                status: SubagentExitStatus::Cancelled,
            }
        });
        // Stop the parent turn before the foreground wait elapses.
        parent_cancel.cancel();
        let result = rx.await.expect("result");
        assert!(matches!(result.status, SubagentExitStatus::Cancelled));
        assert!(check.is_cancelled(), "the parent stop cancelled the child");
    }
}
