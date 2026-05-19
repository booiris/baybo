use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, Message};
use aura_model::{
    ChannelType, ContentBlock, ExternalAgentKind, JobId, Lineage, LineageKind, MessageMetadata,
    SUBAGENT_CHANNEL_TAG, Session, SessionId, SpanId, SubagentBackend, SubagentExitStatus,
    SubagentResult, SubagentSpawnRequest, User,
};
use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::actor::router::Router;
use crate::actor::subagent::await_subagent_terminal;
use crate::external_agent::{ExternalAgent, ExternalAgentEvent, ExternalAgentRequest};

/// `output_tx` buffer for a subagent's actor. Intentionally smaller than
/// the operator-configured channel size for top-level actors — a child
/// session only emits its final `AgentOutput::Message` (deltas are not
/// forwarded back through this channel), so 64 is overkill but matches
/// the wait routine's earlier sizing.
const SUBAGENT_OUTPUT_BUFFER: usize = 64;

impl Router {
    pub(super) async fn handle_subagent_spawn(
        &mut self,
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        let parent = match self.session_manager.get(&parent_session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                let _ = result_tx.send(SubagentResult::failed(format!(
                    "parent session {parent_session_id} not found"
                )));
                return Ok(());
            }
            Err(e) => {
                let _ = result_tx.send(SubagentResult::failed(format!("load parent session: {e}")));
                return Ok(());
            }
        };

        match request.backend.clone() {
            SubagentBackend::Aura { llm } => {
                self.spawn_aura_subagent(
                    parent,
                    parent_job_id,
                    parent_span_id,
                    parent_actor_token,
                    request,
                    llm,
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

    #[allow(clippy::too_many_arguments)]
    async fn spawn_aura_subagent(
        &mut self,
        parent: Session,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        llm: Option<String>,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
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
                return Ok(());
            }
        };

        // Subscribe to terminal events BEFORE dispatch so a child whose
        // actor exits synchronously cannot terminate between
        // dispatch and the receiver being open.
        let terminal_rx = self.job_lifecycle.subscribe_terminal_events();

        let now = Utc::now();
        let incoming = IncomingMessage {
            message: Message {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: child_session.id.clone(),
                channel: child_session.channel.clone(),
                sender: child_session.user.clone(),
                content: vec![ContentBlock::Text(request.initial_prompt())],
                timestamp: now,
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
            platform_msg_id: String::new(),
        };
        let child_session_id = child_session.id.clone();
        let timeout = request.timeout;

        let (output_tx, output_rx) = mpsc::channel::<AgentOutput>(SUBAGENT_OUTPUT_BUFFER);
        let (mailbox, actor_token) =
            self.spawn_oneshot_actor(child_session, llm, output_tx, &parent_actor_token);

        if let Err(e) = mailbox
            .send(AgentMessage::SubagentSpawned {
                initial_message: Box::new(incoming),
                parent_job_id,
            })
            .await
        {
            let _ = result_tx.send(SubagentResult::failed(format!("dispatch child input: {e}")));
            return Ok(());
        }

        let job_lifecycle = Arc::clone(&self.job_lifecycle);
        tokio::spawn(async move {
            let result = await_subagent_terminal(
                child_session_id,
                output_rx,
                terminal_rx,
                mailbox,
                actor_token,
                timeout,
                job_lifecycle,
            )
            .await;
            let _ = result_tx.send(result);
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_external_subagent(
        &mut self,
        parent: Session,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        kind: ExternalAgentKind,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        let Some(agent) = self.external_agents.get(kind) else {
            let _ = result_tx.send(SubagentResult::failed(format!(
                "external agent {:?} is not registered (check `aura external-agent` config + boot logs)",
                kind.as_str(),
            )));
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

        let actor_token = parent_actor_token.child_token();
        let task = request.initial_prompt();
        let timeout = request.timeout;
        let child_session_id = child_session.id.clone();
        let session_manager = Arc::clone(&self.session_manager);

        tokio::spawn(async move {
            let result = run_external_agent(
                agent,
                kind,
                ExternalAgentRequest {
                    task,
                    workspace_dir,
                    resume_key,
                    cancel: actor_token,
                    timeout,
                },
                child_session_id.clone(),
                session_manager,
            )
            .await;
            let _ = result_tx.send(result);
        });
        Ok(())
    }

    /// Returns the child Session — loading an existing one when
    /// `resume_session_id` is set, otherwise minting a new
    /// `LineageKind::Subagent` row. Backend-mismatch + parent +
    /// hidden + lineage checks all run on the resume path.
    async fn resolve_child_session(
        &mut self,
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
            if let Err(reason) = validate_resume_session(&child, parent, backend) {
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
            // workspace_dir is identity, not per-call state: resolve
            // once here from request.workspace_name (or child-id
            // fallback) and reuse verbatim on every resume.
            child.state.subagent_backend = Some(match backend {
                aura_model::SubagentBackendKind::Aura => aura_model::SubagentBackendTag::Aura,
                aura_model::SubagentBackendKind::External(kind) => {
                    aura_model::SubagentBackendTag::External {
                        external_kind: kind,
                        workspace_dir: request
                            .workspace_name
                            .clone()
                            .unwrap_or_else(|| child.id.as_ref().to_string()),
                        resume_key: None,
                    }
                }
            });
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

fn validate_resume_session(
    child: &Session,
    parent: &Session,
    backend: aura_model::SubagentBackendKind,
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

async fn run_external_agent(
    agent: Arc<dyn ExternalAgent>,
    kind: ExternalAgentKind,
    request: ExternalAgentRequest,
    child_session_id: SessionId,
    session_manager: Arc<aura_session::SessionManager>,
) -> SubagentResult {
    let cancel = request.cancel.clone();
    let mut stream = match agent.run(request).await {
        Ok(s) => s,
        Err(e) => {
            return SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Failed(format!("external agent run: {e}")),
            };
        }
    };

    let mut final_content: Option<Vec<ContentBlock>> = None;
    let mut final_status: Option<SubagentExitStatus> = None;

    // The parser inside the agent already enforces the timeout against
    // its own deadline. Adding a second wall-clock deadline here would
    // (a) double the effective budget on cancel/timeout and (b) fire
    // during long reasoning pauses where the parser is correctly idle.
    // The cancel token is the only outer signal we need.
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
            Some(Ok(ExternalAgentEvent::TextDelta(_) | ExternalAgentEvent::Usage(_))) => {
                // Deltas + token usage not surfaced to the parent yet.
                // The agent's final transcript reaches the parent via
                // the FinalContent event below; cost ledger
                // integration is a future task.
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
            Some(Ok(ExternalAgentEvent::FinalContent(blocks))) => {
                final_content = Some(blocks);
                final_status = Some(SubagentExitStatus::Completed);
                break;
            }
            Some(Err(e)) => {
                let msg = e.to_string();
                final_status = Some(if msg.contains("exceeded declared timeout") {
                    SubagentExitStatus::Timeout
                } else {
                    SubagentExitStatus::Failed(msg)
                });
                break;
            }
            None => {
                if final_status.is_none() {
                    final_status = Some(SubagentExitStatus::Failed(
                        "external agent stream ended without FinalContent".into(),
                    ));
                }
                break;
            }
        }
    }

    SubagentResult {
        child_session_id,
        final_content,
        status: final_status.unwrap_or(SubagentExitStatus::Completed),
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

#[cfg(test)]
mod resume_validation_tests {
    use super::*;
    use aura_model::{
        ChannelType, JobId, SessionState, SpanId, TriggerSource, User,
    };
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
            bound_soul_version: "soul-v1".into(),
            hidden: false,
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
        let err = validate_resume_session(&child, &parent, aura_request())
            .expect_err("hidden child must reject");
        assert!(err.contains("hidden"), "got: {err}");
    }

    #[test]
    fn rejects_child_without_lineage() {
        let parent = mk_parent("p");
        let mut child = mk_parent("c");
        child.lineage = None;
        let err = validate_resume_session(&child, &parent, aura_request())
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
        let err = validate_resume_session(&child, &parent, aura_request())
            .expect_err("foreign-parent child must reject");
        assert!(err.contains("different parent"), "got: {err}");
    }

    #[test]
    fn rejects_non_subagent_lineage_kind() {
        let parent = mk_parent("p");
        let child = mk_child(
            "c",
            "p",
            LineageKind::SystemMaintenance,
            Some(aura_tag()),
        );
        let err = validate_resume_session(&child, &parent, aura_request())
            .expect_err("non-Subagent lineage must reject");
        assert!(err.contains("Subagent lineage"), "got: {err}");
    }

    #[test]
    fn rejects_child_without_backend_tag() {
        // Pre-tag sessions (or any row that bypassed the spawn router)
        // must be refused — resume validation depends on the durable
        // tag, no inference fallback.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, None);
        let err = validate_resume_session(&child, &parent, aura_request())
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
        let err = validate_resume_session(&child, &parent, aura_request())
            .expect_err("backend mismatch must reject");
        assert!(err.contains("backend="), "got: {err}");
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn external_rejects_aura_tagged_child() {
        // Reverse mismatch: child tagged Aura, resumed as External.
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        let err = validate_resume_session(&child, &parent, claude_request())
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
        validate_resume_session(&child, &parent, claude_request())
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
        let err = validate_resume_session(&child, &parent, claude_request())
            .expect_err("missing resume_key must reject");
        assert!(err.contains("no persisted"), "got: {err}");
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn aura_accepts_fresh_aura_child() {
        let parent = mk_parent("p");
        let child = mk_child("c", "p", LineageKind::Subagent, Some(aura_tag()));
        validate_resume_session(&child, &parent, aura_request())
            .expect("aura→aura with matching tag must accept");
    }
}
