//! Async per-session summary refresh handler invoked by the
//! maintenance actor's `AgentMessage::SystemTrigger` path. A single
//! tool-free LLM call wrapped in a `StepKind::Compression` +
//! `LlmCall` span; cost is recorded against the maintenance session.
//! See `docs/context-summary-refresh.md`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use aura_context::{ContextError, parse_summary_response};
use aura_llm::{ChatRequest, GuardedLlm, LlmResponse, ModelInfo};
use aura_model::{ChatMessage, ContentBlock, JobId, Role, SessionId};
use aura_session::SessionManager;
use aura_trace::{LifecycleOutcome, LlmCallBegin, LlmCallInputs, LlmCallResult, StepKind};
use aura_workspace::WorkspacePaths;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::trace::SpanRecorder;

/// Payload carried by `JobInput::System { reason: SummaryRefresh, payload }`.
/// The parent's agent loop builds this at trigger spawn time;
/// the maintenance session's actor parses it via `serde_json::from_value`
/// before invoking the refresher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryRefreshPayload {
    pub parent_session_id: SessionId,
    /// Highest `session_messages.ordinal` to include in this pass'
    /// input. Pinned at trigger time so concurrent appends to the
    /// parent don't bleed in mid-pass.
    pub up_to_ordinal: i64,
}

/// Result of one refresh pass; carried back as the
/// `JobOutput::Structured.value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SummaryRefreshOutcome {
    pub cursor: i64,
    pub model_id: String,
    pub span_id: String,
    pub cost_micros: i64,
}

/// The trailing instruction appended to the parent's transcript
/// before the LLM call. Mirrors today's `SUMMARIZE_INSTRUCTION`
/// shape (analysis + summary blocks for parsing) with an explicit
/// preamble that establishes Pattern B's contract: conversation is
/// authoritative, prior summary is scaffolding, size target ~8-12K.
fn build_summary_prompt(prior_summary: Option<&str>) -> String {
    let prior = match prior_summary {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "(none — this is the first pass)".to_string(),
    };

    format!(
        "CONTEXT: A previous summary of part of this conversation exists and is provided \
below for terminology and structural consistency. The conversation transcript above is \
the authoritative source — re-derive every fact from it. Only use the prior summary as \
a scaffold to keep names, file paths, and concept labels stable across passes.\n\n\
PRIOR SUMMARY:\n{prior}\n\n---\n\n\
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n\
Your task is to create a detailed summary of the conversation so far, paying close \
attention to the user's explicit requests and your previous actions. This summary \
should be thorough in capturing technical details, code patterns, and architectural \
decisions that would be essential for continuing development work without losing \
context.\n\n\
Before providing your final summary, wrap your analysis in <analysis> tags. Then \
produce a single <summary>...</summary> block with these sections:\n\n\
1. Primary Request and Intent\n\
2. Key Technical Concepts\n\
3. Files and Code Sections\n\
4. Errors and fixes\n\
5. Problem Solving\n\
6. All user messages\n\
7. Pending Tasks\n\
8. Current Work\n\
9. Optional Next Step\n\n\
SIZE TARGET: aim for ~8-12K tokens. Grow when genuinely more substance has \
accumulated; do not pad."
    )
}

/// Atomic write — tempfile + rename. Creates the per-session
/// directory if absent. Same crash-safety guarantee as
/// `aura_workspace::identity::write_identity_file`: readers see
/// either the prior file or the new one, never a partial.
async fn atomic_write_summary(
    paths: &WorkspacePaths,
    parent_session_id: &SessionId,
    body: &str,
) -> std::io::Result<PathBuf> {
    let dir = paths.session_state_dir(parent_session_id.as_str());
    tokio::fs::create_dir_all(&dir).await?;
    let target = paths.session_summary_file(parent_session_id.as_str());
    let tmp = paths.session_summary_tmp_file(parent_session_id.as_str());
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, &target).await?;
    Ok(target)
}

/// Bootstrap-supplied callback that creates a new maintenance
/// session lineaged to `parent` and spawns an actor for it,
/// then sends an `AgentMessage::SystemTrigger` carrying
/// `JobInput::System { reason: SummaryRefresh, payload }` to the
/// new actor's mailbox. Fire-and-forget — the parent's gate just
/// records the intent; the spawner handles the rest. Returns once
/// the actor has been spawned and the message dispatched (not when
/// the refresh completes).
///
/// Implemented by the bootstrap layer where `SessionManager` +
/// `ActorSpawner` converge. `AgentLoop` holds an
/// `Arc<dyn MaintenanceSpawner>` injected at construction; the
/// trigger gate just reads it and calls `spawn_summary_refresh`
/// when the conjunction of conditions is satisfied.
#[async_trait::async_trait]
pub trait MaintenanceSpawner: Send + Sync {
    async fn spawn_summary_refresh(
        &self,
        parent_session_id: SessionId,
        parent_job_id: JobId,
        payload: SummaryRefreshPayload,
    ) -> anyhow::Result<()>;

    /// Send `AgentMessage::Shutdown` to every in-flight maintenance
    /// session lineaged to `parent_session_id`. Invoked by the
    /// parent's `AgentActor::Shutdown` handler before tripping its
    /// own `actor_token`, so the cascade reaches the children.
    /// Errors are logged at the call site; shutdown still proceeds.
    async fn cancel_maintenance_for_parent(
        &self,
        parent_session_id: &SessionId,
    ) -> anyhow::Result<()>;
}

/// Inputs for one refresh pass — all the agent-layer machinery the
/// dedicated handler needs without dragging the entire `AgentLoop`
/// surface in.
pub(crate) struct SummaryRefresher {
    pub llm_client: Arc<GuardedLlm>,
    pub security_gateway: Arc<SecurityGateway>,
    pub cost_manager: Option<Arc<CostManager>>,
    pub sessions: Arc<SessionManager>,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub recorder: Arc<SpanRecorder>,
    pub model_info: ModelInfo,
    pub maintenance_session_id: SessionId,
    pub maintenance_user_id: String,
    pub job_id: JobId,
    pub cancel_token: CancellationToken,
}

impl SummaryRefresher {
    /// Execute one summary refresh pass. Returns the structured
    /// outcome on success; on failure, increments the parent's
    /// `error_count` and returns the underlying error.
    pub async fn run(
        self,
        payload: SummaryRefreshPayload,
    ) -> anyhow::Result<SummaryRefreshOutcome> {
        let SummaryRefresher {
            llm_client,
            security_gateway,
            cost_manager,
            sessions,
            workspace_paths,
            recorder,
            model_info,
            maintenance_session_id,
            maintenance_user_id,
            job_id,
            cancel_token,
        } = self;

        let parent_id = payload.parent_session_id.clone();
        let up_to_ordinal = payload.up_to_ordinal;

        // `up_to_ordinal` pins the snapshot the trigger fired against
        // so concurrent appends to the parent's transcript don't
        // bleed into this pass' input.
        let parent_messages = match load_parent_transcript_up_to(
            sessions.as_ref(),
            &parent_id,
            up_to_ordinal,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&parent_id, &model_info.id, "", Utc::now())
                    .await;
                return Err(e);
            }
        };
        if parent_messages.is_empty() {
            warn!(
                parent_session_id = %parent_id,
                "summary refresh: parent transcript is empty after load — skipping pass"
            );
            return Ok(SummaryRefreshOutcome {
                cursor: up_to_ordinal,
                model_id: model_info.id.clone(),
                span_id: String::new(),
                cost_micros: 0,
            });
        }

        let prior_summary = match read_existing_summary(&workspace_paths, &parent_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    parent_session_id = %parent_id,
                    error = %e,
                    "summary refresh: failed to read existing summary; treating as first-pass"
                );
                None
            }
        };

        let mut request_messages: Vec<ChatMessage> = parent_messages;
        request_messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(build_summary_prompt(
                prior_summary.as_deref(),
            ))],
        });
        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
        };

        let span_outcome = run_llm_with_span(
            &llm_client,
            &recorder,
            &security_gateway,
            cost_manager.clone(),
            &model_info,
            &maintenance_session_id,
            &maintenance_user_id,
            job_id,
            &cancel_token,
            request,
        )
        .await;

        let (response, span_id, cost_micros) = match span_outcome {
            Ok(t) => t,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&parent_id, &model_info.id, "", Utc::now())
                    .await;
                return Err(anyhow::anyhow!(e));
            }
        };

        // Step 5: parse, write to disk, update metadata.
        let summary_text = parse_summary_response(&response.content)
            .ok_or_else(|| anyhow::anyhow!("summary response empty after parsing"))?;

        if let Err(e) = atomic_write_summary(&workspace_paths, &parent_id, &summary_text).await {
            warn!(
                parent_session_id = %parent_id,
                error = %e,
                "summary refresh: atomic file write failed; metadata not updated"
            );
            let _ = sessions
                .record_summary_failure(&parent_id, &model_info.id, &span_id, Utc::now())
                .await;
            return Err(anyhow::anyhow!("atomic write failed: {e}"));
        }

        // On metadata failure the file lands but the row doesn't —
        // an FS orphan that the next successful pass overwrites or
        // the startup reaper cleans up. Return success regardless.
        if let Err(e) = sessions
            .record_summary_success(
                &parent_id,
                up_to_ordinal,
                cost_micros,
                &model_info.id,
                &span_id,
                Utc::now(),
            )
            .await
        {
            warn!(
                parent_session_id = %parent_id,
                error = %e,
                "summary refresh: metadata write failed; FS orphan possible"
            );
        }

        info!(
            parent_session_id = %parent_id,
            cursor = up_to_ordinal,
            cost_micros,
            "summary refresh: pass succeeded"
        );

        Ok(SummaryRefreshOutcome {
            cursor: up_to_ordinal,
            model_id: model_info.id,
            span_id,
            cost_micros,
        })
    }
}

async fn load_parent_transcript_up_to(
    sessions: &SessionManager,
    parent_id: &SessionId,
    up_to_ordinal: i64,
) -> anyhow::Result<Vec<ChatMessage>> {
    // The active-only loader doesn't surface ordinals, so we use the
    // supersede-aware loader and filter for active + within ordinal
    // bound here.
    let rows = sessions
        .load_session_messages_with_supersede(parent_id)
        .await
        .with_context(|| format!("load_session_messages_with_supersede({parent_id})"))?;

    let mut out = Vec::new();
    for row in rows {
        if row.superseded_by.is_some() {
            continue;
        }
        if row.ordinal > up_to_ordinal {
            continue;
        }
        out.push(row.message);
    }
    debug!(
        parent_session_id = %parent_id,
        up_to_ordinal,
        loaded = out.len(),
        "summary refresh: loaded parent transcript slice"
    );
    Ok(out)
}

async fn read_existing_summary(
    paths: &WorkspacePaths,
    parent_id: &SessionId,
) -> std::io::Result<Option<String>> {
    let path = paths.session_summary_file(parent_id.as_str());
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Mirrors `CompressionRunner::run` but charges cost against the
/// maintenance session and returns the cost it billed (so the
/// caller can write it to `session_summaries`).
#[allow(clippy::too_many_arguments)]
async fn run_llm_with_span(
    llm_client: &Arc<GuardedLlm>,
    recorder: &Arc<SpanRecorder>,
    security_gateway: &Arc<SecurityGateway>,
    cost_manager: Option<Arc<CostManager>>,
    model_info: &ModelInfo,
    maintenance_session_id: &SessionId,
    maintenance_user_id: &str,
    job_id: JobId,
    cancel_token: &CancellationToken,
    request: ChatRequest,
) -> Result<(LlmResponse, String, i64), ContextError> {
    let cancel_ctx = Some((cancel_token, aura_job::CancelReason::ParentCancelled));
    let begin = LlmCallBegin {
        model_id: model_info.id.clone(),
        provider: model_info.provider.clone(),
        provider_config_hash: String::new(),
        input_messages: LlmCallInputs::Inline(request.messages.clone()),
        temperature: request.temperature,
    };

    let llm_client = Arc::clone(llm_client);
    let recorder_inner = Arc::clone(recorder);
    let security_gateway = Arc::clone(security_gateway);
    let model_info = model_info.clone();
    let maintenance_session_id = maintenance_session_id.clone();
    let maintenance_user_id = maintenance_user_id.to_string();

    crate::scope::with_step(
        recorder.as_ref(),
        job_id,
        StepKind::Compression,
        cancel_ctx,
        |step| async move {
            let response = crate::scope::with_llm_span(
                recorder_inner.as_ref(),
                &step,
                job_id,
                begin,
                cancel_ctx,
                |span| async move {
                    let span_id_str = span.span_id.to_string();
                    match llm_client.chat(&request).await {
                        Ok(mut response) => {
                            if let Err(e) =
                                security_gateway.sanitize_llm_response(&mut response).await
                            {
                                warn!(error = %e, "failed to sanitize summary refresh response");
                            }
                            let mut cost_micros: i64 = 0;
                            if let Some(cm) = &cost_manager {
                                cm.record_call(
                                    &maintenance_user_id,
                                    maintenance_session_id.clone(),
                                    job_id,
                                    span.span_id,
                                    &model_info.id,
                                    response.usage.input_tokens,
                                    response.usage.output_tokens,
                                    response.usage.cached_input_tokens,
                                    response.usage.cache_creation_input_tokens,
                                );
                                cost_micros = cm.cost_micros_for(
                                    &model_info.id,
                                    response.usage.input_tokens,
                                    response.usage.output_tokens,
                                );
                            }
                            let call_result = LlmCallResult {
                                output_content: response.content.clone(),
                                thinking: response.thinking.clone(),
                                tool_calls: vec![],
                                input_tokens: response.usage.input_tokens,
                                output_tokens: response.usage.output_tokens,
                                cached_input_tokens: response.usage.cached_input_tokens,
                                cache_creation_input_tokens: response
                                    .usage
                                    .cache_creation_input_tokens,
                            };
                            (call_result, Ok((response, span_id_str, cost_micros)))
                        }
                        Err(e) => {
                            let raw = e.to_string();
                            let error_msg =
                                security_gateway.sanitize_error(&raw).await.unwrap_or(raw);
                            let call_result = LlmCallResult {
                                output_content: String::new(),
                                thinking: None,
                                tool_calls: vec![],
                                input_tokens: 0,
                                output_tokens: 0,
                                cached_input_tokens: 0,
                                cache_creation_input_tokens: 0,
                            };
                            (call_result, Err(anyhow::anyhow!(error_msg)))
                        }
                    }
                },
            )
            .await?;
            Ok((LifecycleOutcome::Ok, response))
        },
    )
    .await
    .map_err(|e| ContextError::Compression(e.to_string()))
}

/// Startup orphan reaper. Runs once per process boot, *before* the
/// supervisor starts spawning actors. Two responsibilities:
///
/// 1. **DB-side** — delete every existing maintenance session row
///    (`is_normal_session = 0`). These represent in-flight passes
///    that were running when the previous process crashed; their
///    actors are gone, their state is stateless by design, and
///    their parents' `error_count` should reflect the failed pass
///    so operators see the trace.
/// 2. **FS-side** — scan
///    `<workspace>/state/sessions/*/summary.md` and delete any file
///    whose `session_id` has no corresponding row in
///    `session_summaries`. Removes orphans left by metadata-write
///    failures (ρ-1 Option A).
///
/// Best-effort: errors are logged at warn but never propagate; a
/// flaky filesystem must not block process boot.
pub async fn reap_maintenance_orphans(
    sessions: &SessionManager,
    workspace_paths: &aura_workspace::WorkspacePaths,
) {
    // ---- DB orphans ---------------------------------------------------
    let maintenance_ids = match sessions.all_maintenance_sessions().await {
        Ok(ids) => ids,
        Err(e) => {
            warn!(error = %e, "orphan reap: list_all_maintenance_sessions failed");
            Vec::new()
        }
    };
    for id in &maintenance_ids {
        // Find the parent (lineage may be missing; fall back to no-op)
        // and bump its `session_summaries.error_count` so failed
        // passes surface in telemetry. The maintenance session
        // itself gets deleted regardless.
        if let Ok(Some(maint)) = sessions.get(id).await
            && let Some(parent_id) = maint.lineage.as_ref().map(|l| l.parent_session_id.clone())
            && let Err(e) = sessions
                .record_summary_failure(&parent_id, "orphan-reap", "", chrono::Utc::now())
                .await
        {
            warn!(
                parent_session_id = %parent_id,
                error = %e,
                "orphan reap: bump_error_count failed for parent"
            );
        }
        if let Err(e) = sessions.delete(id).await {
            warn!(
                maintenance_session_id = %id,
                error = %e,
                "orphan reap: delete maintenance session failed"
            );
        } else {
            debug!(maintenance_session_id = %id, "orphan reap: deleted maintenance session");
        }
    }
    if !maintenance_ids.is_empty() {
        info!(
            reaped = maintenance_ids.len(),
            "orphan reap: deleted in-flight maintenance sessions from previous boot"
        );
    }

    // ---- FS orphans ---------------------------------------------------
    let known_ids = match sessions
        .summary_store()
        .map(|s| async move { s.list_session_ids().await })
    {
        Some(fut) => match fut.await {
            Ok(ids) => ids
                .into_iter()
                .map(|i| i.as_str().to_string())
                .collect::<std::collections::HashSet<_>>(),
            Err(e) => {
                warn!(error = %e, "orphan reap: list_session_ids failed; FS sweep skipped");
                return;
            }
        },
        None => {
            // No summary store wired — nothing to reconcile against.
            return;
        }
    };

    let sessions_dir = workspace_paths.state_sessions_dir();
    let mut entries = match tokio::fs::read_dir(&sessions_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(
                dir = %sessions_dir.display(),
                error = %e,
                "orphan reap: read_dir failed; FS sweep skipped"
            );
            return;
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if known_ids.contains(&name) {
            continue;
        }
        let dir = entry.path();
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            warn!(
                dir = %dir.display(),
                error = %e,
                "orphan reap: remove_dir_all failed"
            );
        } else {
            debug!(dir = %dir.display(), "orphan reap: deleted FS orphan summary directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_summary_block_after_stripping_analysis() {
        let s = "<analysis>thinking</analysis><summary>S</summary>";
        assert_eq!(
            parse_summary_response(s).as_deref(),
            Some("<summary>S</summary>")
        );
    }

    #[test]
    fn parse_returns_leftover_when_summary_tag_absent() {
        let s = "<analysis>thinking</analysis>\nbody text";
        assert_eq!(parse_summary_response(s).as_deref(), Some("body text"));
    }

    #[test]
    fn parse_returns_none_when_only_analysis() {
        let s = "<analysis>x</analysis>";
        assert!(parse_summary_response(s).is_none());
    }

    #[test]
    fn payload_round_trips_through_value() {
        let p = SummaryRefreshPayload {
            parent_session_id: SessionId::from("user-1"),
            up_to_ordinal: 42,
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: SummaryRefreshPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.parent_session_id, p.parent_session_id);
        assert_eq!(back.up_to_ordinal, p.up_to_ordinal);
    }

    #[test]
    fn build_prompt_includes_size_target_and_prior_marker() {
        let prompt = build_summary_prompt(None);
        assert!(prompt.contains("SIZE TARGET"));
        assert!(prompt.contains("(none — this is the first pass)"));

        let prompt2 = build_summary_prompt(Some("OLD SUMMARY BODY"));
        assert!(prompt2.contains("OLD SUMMARY BODY"));
        assert!(!prompt2.contains("(none —"));
    }
}
