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
//! - [`BackgroundCompressionRunner`] is the in-actor background pass:
//!   it gathers the agent-layer deps, adapts `CompressionRunner` to the
//!   context callback shape, and delegates the rest of the flow to
//!   [`aura_context::run_background_summary`]. The pass is spawned
//!   detached by [`crate::runtime::agent_loop::AgentLoop`].
//! - [`reap_orphan_summaries`] is the startup cleanup pass that
//!   removes FS-orphan summary directories from a previous boot.
//!
//! See `docs/background-compression.md`.

use std::sync::Arc;

use aura_context::{
    BackgroundSummaryConfig, BackgroundSummaryOutcome, Tokenizer, run_background_summary,
};
use aura_llm::{Attribution, BillableLlm, ModelInfo};
use aura_model::{BackgroundCompressionPayload, JobId, SessionId};
use aura_session::SessionManager;
use aura_trace::{LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanRecorder, StepKind};
use aura_workspace::WorkspacePaths;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::security::SecurityGateway;

/// Agent-side dependencies needed to execute the compression LLM call:
/// trace recorder, cost ledger, the LLM client, and the identity /
/// cancel context that pin the call to a specific job + session.
pub(crate) struct CompressionRunner {
    pub(crate) llm_client: Arc<BillableLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
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
        input_marker: aura_trace::LlmCallInputs,
    ) -> Result<aura_context::SummaryChatRun, aura_context::ContextError> {
        let CompressionRunner {
            llm_client,
            recorder,
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
            // Marker supplied by the caller: a `Persisted` ordinal
            // reference to the transcript prefix (so the span doesn't
            // re-embed the whole summarized window) plus an inline
            // suffix for the framing/sub-loop turns that aren't in
            // `session_messages`. The compressor owns the split because
            // it's the layer that knows which slice is persisted.
            input_messages: input_marker,
            temperature: request.temperature,
            goal_steering: None,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::runtime::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::Compression,
            cancel_ctx,
            |step| async move {
                let result = crate::runtime::scope::with_llm_span(
                    recorder_inner.as_ref(),
                    &step,
                    job_id,
                    begin,
                    cancel_ctx,
                    |span| async move {
                        let span_id_str = span.span_id.to_string();
                        let bound = llm_client.bind(Attribution {
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            job_id,
                            span_id: span.span_id,
                            reason: aura_llm::CallReason::Compression,
                        });
                        match bound.chat(&request).await {
                            Ok(billed) => {
                                let mut response = billed.response;
                                // Scrub before the summary lands in the
                                // trace, the cost row's joinable content,
                                // or the re-injected [Conversation
                                // Summary]. Placeholders are kept, not
                                // revealed — the summarized brain operates
                                // against the sanitized transcript.
                                if let Err(e) =
                                    security_gateway.sanitize_llm_response(&mut response).await
                                {
                                    warn!(error = %e, "compression: sanitize_llm_response failed");
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
                                (
                                    call_result,
                                    Ok(aura_context::SummaryChatRun {
                                        response,
                                        span_id: span_id_str,
                                        cost_micros: billed.cost_micros.into_micros(),
                                    }),
                                )
                            }
                            Err(e) => {
                                let raw = e.to_string();
                                let msg =
                                    security_gateway.sanitize_error(&raw).await.unwrap_or(raw);
                                (LlmCallResult::default(), Err(anyhow::anyhow!(msg)))
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
    pub llm_client: Arc<BillableLlm>,
    pub security_gateway: Arc<SecurityGateway>,
    pub sessions: Arc<SessionManager>,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub tokenizer: Arc<dyn Tokenizer>,
    pub recorder: Arc<SpanRecorder>,
    pub model_info: ModelInfo,
    /// The session the pass runs on behalf of — also the session its
    /// cost + `StepKind::Compression` + `LlmCall` spans attribute to.
    pub session_id: SessionId,
    pub user_id: String,
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
            sessions,
            workspace_paths,
            tokenizer,
            recorder,
            model_info,
            session_id,
            user_id,
            job_id,
            cancel_token,
        } = self;
        let model_id = model_info.id.clone();
        // Clone the session id for the summary config — the chat
        // closure below moves the destructured field.
        let summary_session_id = session_id.clone();
        // Hand a clone to the context's tool loop so a cancel
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
        let chat: aura_context::BackgroundSummaryCallback = Box::new(move |req, marker| {
            let runner = CompressionRunner {
                llm_client: llm_client.clone(),
                recorder: recorder.clone(),
                security_gateway: security_gateway.clone(),
                job_id,
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                model_info: model_info.clone(),
                cancel_token: cancel_token.clone(),
            };
            Box::pin(async move { runner.run(req, marker).await })
        });

        let config = BackgroundSummaryConfig {
            workspace: workspace_paths,
            sessions,
            tokenizer,
            session_id: summary_session_id,
            up_to_ordinal: payload.up_to_ordinal,
            model_id,
            cancel_token: tool_cancel,
        };
        run_background_summary(config, chat)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Startup FS orphan reaper. Runs once per process boot, *before* the
/// supervisor starts spawning actors.
///
/// Scans `<workspace>/state/sessions/*/` and deletes any directory whose
/// `session_id` has no corresponding row in `session_summaries`. This
/// removes summary dirs left by an `Edit`-wrote-the-file-then-crashed
/// pass (the on-disk `summary.md` lands before the metadata row commits).
///
/// A leftover orphan dir is otherwise harmless (the fast-path reads
/// `summary_metadata == None` and falls through), but the sweep keeps the
/// workspace tidy.
///
/// Best-effort: errors are logged at warn but never propagate; a
/// flaky filesystem must not block process boot.
pub async fn reap_orphan_summaries(sessions: &SessionManager, workspace_paths: &WorkspacePaths) {
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
