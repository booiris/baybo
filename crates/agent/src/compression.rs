//! Agent-side wiring for both context-compression flows.
//!
//! `aura-context` deliberately doesn't know about `SpanRecorder`,
//! `CostManager`, or `JobId` — those are agent-layer concerns. The
//! `ContextManager::maybe_compress` and `aura_context::run_background_summary`
//! APIs both take chat callbacks so the caller can inject all of that
//! cross-cutting machinery without polluting the context crate.
//!
//! This module owns both injections:
//!
//! - [`CompressionRunner`] bundles every dependency the compression
//!   LLM call needs and exposes a single `run(self, req)` method that
//!   the agent loop hands to `maybe_compress` (inline path) and to
//!   [`BackgroundCompressionRunner`] (background path).
//! - [`BackgroundCompressionRunner`] is the maintenance-actor's entry
//!   point: it gathers the agent-layer deps, adapts
//!   `CompressionRunner` to the context callback shape, and delegates
//!   the rest of the flow to [`aura_context::run_background_summary`].
//! - [`reap_maintenance_orphans`] is the startup cleanup pass that
//!   removes leftover maintenance sessions and FS-orphan summaries
//!   from a previous boot.
//!
//! The `SystemSpawnRequest` channel contract that ferries
//! background-compression triggers from the parent's gate to the
//! router lives in [`crate::router`] — it's the router's surface, not
//! a compression concern.
//!
//! See `docs/background-compression.md`.

use std::sync::Arc;

use aura_context::{
    BackgroundSummaryConfig, BackgroundSummaryOutcome, Tokenizer, run_background_summary,
};
use aura_llm::{GuardedLlm, ModelInfo};
use aura_model::{BackgroundCompressionPayload, JobId, SessionId};
use aura_session::SessionManager;
use aura_trace::{LifecycleOutcome, LlmCallBegin, LlmCallResult, StepKind};
use aura_workspace::WorkspacePaths;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::billed_chat::{BilledAttribution, chat_billed_core};
use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::trace::SpanRecorder;

/// Synthetic `model_id` recorded against `session_summaries.error_count`
/// when the orphan reaper bumps a parent's failure count for a
/// crashed-mid-pass maintenance session.
const ORPHAN_REAP_MODEL_TAG: &str = "orphan-reap";

/// Agent-side dependencies needed to execute the compression LLM call:
/// trace recorder, cost ledger, the LLM client, and the identity /
/// cancel context that pin the call to a specific job + session.
pub(crate) struct CompressionRunner {
    pub(crate) llm_client: Arc<GuardedLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    pub(crate) cost_manager: Arc<CostManager>,
    /// Same gateway as the main LLM path. Compression LLM output and
    /// errors are scrubbed through it before they land in the trace,
    /// the cost record's joinable content, or the [Conversation
    /// Summary] re-injected into the session — otherwise a model
    /// tricked by prompt injection in the messages it's summarizing
    /// could leak secret-like text into persisted state.
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) job_id: JobId,
    pub(crate) user_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) model_info: ModelInfo,
    pub(crate) cancel_token: CancellationToken,
}

impl CompressionRunner {
    /// Execute the compression LLM call. Brackets it in a
    /// `StepKind::Compression` step + `LlmCall` span (real lifecycle —
    /// not a post-hoc placeholder), records the cost row against the
    /// span_id while the span is open, and returns the sanitized
    /// `LlmResponse` along with the span id + billed cost. The inline
    /// (`maybe_compress`) closure typically discards the metadata; the
    /// background-compression path consumes both.
    ///
    /// `maybe_compress` then trims the content and applies the
    /// strategy's `on_failure` slice if the response is empty or this
    /// method returns `Err`.
    pub(crate) async fn run(
        self,
        request: aura_llm::ChatRequest,
    ) -> Result<aura_context::SummaryChatRun, aura_context::ContextError> {
        let CompressionRunner {
            llm_client,
            recorder,
            cost_manager,
            security_gateway,
            job_id,
            user_id,
            session_id,
            model_info,
            cancel_token,
        } = self;

        let cancel_ctx = Some((&cancel_token, aura_job::CancelReason::ParentCancelled));
        let begin = LlmCallBegin {
            model_id: model_info.id.clone(),
            provider: model_info.provider.clone(),
            provider_config_hash: String::new(),
            // Compression LLM input is a one-off slice the caller
            // built off the active transcript — it never lands in
            // `session_messages`, so embed inline rather than
            // referencing an ordinal that doesn't exist.
            input_messages: aura_trace::LlmCallInputs::Inline(request.messages.clone()),
            temperature: request.temperature,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::Compression,
            cancel_ctx,
            |step| async move {
                let result = crate::scope::with_llm_span(
                    recorder_inner.as_ref(),
                    &step,
                    job_id,
                    begin,
                    cancel_ctx,
                    |span| async move {
                        let span_id_str = span.span_id.to_string();
                        let attribution = BilledAttribution {
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            job_id,
                            span_id: span.span_id,
                        };
                        match chat_billed_core(
                            &llm_client,
                            &security_gateway,
                            &cost_manager,
                            &model_info,
                            &attribution,
                            &request,
                        )
                        .await
                        {
                            Ok(run) => {
                                let call_result = LlmCallResult {
                                    output_content: run.response.content.clone(),
                                    thinking: run.response.thinking.clone(),
                                    tool_calls: vec![],
                                    input_tokens: run.response.usage.input_tokens,
                                    output_tokens: run.response.usage.output_tokens,
                                    cached_input_tokens: run.response.usage.cached_input_tokens,
                                    cache_creation_input_tokens: run
                                        .response
                                        .usage
                                        .cache_creation_input_tokens,
                                };
                                (
                                    call_result,
                                    Ok(aura_context::SummaryChatRun {
                                        response: run.response,
                                        span_id: span_id_str,
                                        cost_micros: run.cost_micros.into_micros(),
                                    }),
                                )
                            }
                            Err(error_msg) => {
                                (LlmCallResult::default(), Err(anyhow::anyhow!(error_msg)))
                            }
                        }
                    },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, result))
            },
        )
        .await
        .map_err(|e| aura_context::ContextError::Compression(e.to_string()))
    }
}

/// Inputs for one background-summary pass — all the agent-layer
/// machinery the dedicated handler needs without dragging the entire
/// `AgentLoop` surface in. The actual flow lives in
/// [`aura_context::run_background_summary`]; this struct is the
/// agent-side adapter.
pub(crate) struct BackgroundCompressionRunner {
    pub llm_client: Arc<GuardedLlm>,
    pub security_gateway: Arc<SecurityGateway>,
    pub cost_manager: Arc<CostManager>,
    pub sessions: Arc<SessionManager>,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub tokenizer: Arc<dyn Tokenizer>,
    pub recorder: Arc<SpanRecorder>,
    pub model_info: ModelInfo,
    pub maintenance_session_id: SessionId,
    pub maintenance_user_id: String,
    pub job_id: JobId,
    pub cancel_token: CancellationToken,
}

impl BackgroundCompressionRunner {
    /// Execute one background-summary pass. Adapts the agent-layer
    /// `CompressionRunner` to the context callback shape and delegates
    /// the rest of the flow to [`aura_context::run_background_summary`].
    pub async fn run(
        self,
        payload: BackgroundCompressionPayload,
    ) -> anyhow::Result<BackgroundSummaryOutcome> {
        let BackgroundCompressionRunner {
            llm_client,
            security_gateway,
            cost_manager,
            sessions,
            workspace_paths,
            tokenizer,
            recorder,
            model_info,
            maintenance_session_id,
            maintenance_user_id,
            job_id,
            cancel_token,
        } = self;
        let model_id = model_info.id.clone();
        // Hand a clone to the context's tool loop so a parent cancel
        // cascades into in-flight `Read`/`Edit`. The original token
        // moves into the chat-callback closure below, where each
        // per-iteration `CompressionRunner` clones it again for the
        // LLM-call cancel.
        let tool_cancel = cancel_token.clone();

        // FnMut closure: each iteration of the background-summary
        // tool-loop fires its own LLM call, so we can't move a single
        // `CompressionRunner` into the closure (its `run` consumes
        // self). Capture the agent-layer `Arc`s + cheap `Clone` fields
        // outside; the closure rebuilds a fresh runner per call. The
        // runner's per-call `Compression` step + `LlmCall` span are
        // each treated as one iteration in telemetry.
        let chat: aura_context::BackgroundSummaryCallback = Box::new(move |req| {
            let runner = CompressionRunner {
                llm_client: llm_client.clone(),
                recorder: recorder.clone(),
                cost_manager: cost_manager.clone(),
                security_gateway: security_gateway.clone(),
                job_id,
                user_id: maintenance_user_id.clone(),
                session_id: maintenance_session_id.clone(),
                model_info: model_info.clone(),
                cancel_token: cancel_token.clone(),
            };
            Box::pin(async move { runner.run(req).await })
        });

        let config = BackgroundSummaryConfig {
            workspace: workspace_paths,
            sessions,
            tokenizer,
            parent_session_id: payload.parent_session_id,
            up_to_ordinal: payload.up_to_ordinal,
            model_id,
            cancel_token: tool_cancel,
        };
        run_background_summary(config, chat)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Startup orphan reaper. Runs once per process boot, *before* the
/// supervisor starts spawning actors. Two responsibilities:
///
/// 1. **DB-side** — delete only the maintenance session rows
///    (`is_normal_session = 0`) whose associated job is **not** in a
///    terminal state. Those represent in-flight passes that were
///    running when the previous process crashed; their actors are
///    gone, their state is stateless by design, and their parents'
///    `error_count` should reflect the failed pass so operators see
///    the trace. Maintenance sessions whose pass landed cleanly
///    (`jobs.status_kind` ∈ {`completed`, `failed`, `cancelled`})
///    are kept as audit history — cost-records joins depend on them.
/// 2. **FS-side** — scan
///    `<workspace>/state/sessions/*/summary.md` and delete any file
///    whose `session_id` has no corresponding row in
///    `session_summaries`. Removes orphans left by metadata-write
///    failures (ρ-1 Option A).
///
/// Best-effort: errors are logged at warn but never propagate; a
/// flaky filesystem must not block process boot.
pub async fn reap_maintenance_orphans(sessions: &SessionManager, workspace_paths: &WorkspacePaths) {
    // ---- Stale in_flight sweep ----------------------------------------
    // A process that just started has no in-flight pass by definition,
    // so any `session_summaries.in_flight = 1` left from the previous
    // boot is stale. The maintenance-session sweep below also clears
    // `in_flight` (via `record_summary_failure` → `bump_error_count`)
    // for parents whose maintenance row survived, but cannot recover
    // the case where the trigger gate marked `in_flight = 1` *before*
    // the router created the maintenance session row and the process
    // died in that window. Run this first, idempotent.
    if let Err(e) = sessions.clear_all_summary_in_flight().await {
        warn!(
            error = %e,
            "orphan reap: clear_all_summary_in_flight failed"
        );
    }

    // ---- DB orphans ---------------------------------------------------
    // `unfinished_maintenance_sessions()` excludes rows whose job
    // reached a terminal state — those are kept as audit history.
    let maintenance_ids = match sessions.unfinished_maintenance_sessions().await {
        Ok(ids) => ids,
        Err(e) => {
            warn!(error = %e, "orphan reap: list_unfinished_maintenance_sessions failed");
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
                .record_summary_failure(&parent_id, ORPHAN_REAP_MODEL_TAG, "", chrono::Utc::now())
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
            "orphan reap: deleted unfinished maintenance sessions from previous boot"
        );
    }

    let summary_store = sessions.summary_store();
    let known_ids: std::collections::HashSet<String> = match summary_store.list_session_ids().await
    {
        Ok(ids) => ids.into_iter().map(|i| i.as_str().to_string()).collect(),
        Err(e) => {
            warn!(error = %e, "orphan reap: list_session_ids failed; FS sweep skipped");
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
