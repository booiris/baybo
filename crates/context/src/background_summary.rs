//! Background summary pass.
//!
//! Iterative pass driven from a maintenance session: load the parent's
//! transcript up to a pinned ordinal, send `[transcript +
//! build_summary_prompt(prior_summary)]` to the LLM, atomic-write the
//! parsed body back to `<workspace>/state/sessions/<parent>/summary.md`,
//! and record success / failure on `session_summaries`.
//!
//! [`run_background_summary`] is the entry point. The chat callback is
//! the only agent-layer-coupled piece — it bundles the LLM client +
//! cost + trace recording the inline path also uses, and returns the
//! LLM response together with the span id and billed cost. Everything
//! else (transcript load, prior-summary read, prompt build, parse,
//! atomic write, metadata record) lives here next to the inline-flow
//! code in [`crate::compressor`] that reads back the same `summary.md`
//! via the fast-path.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aura_llm::{ChatRequest, LlmResponse};
use aura_model::{ChatMessage, ContentBlock, Role, SessionId};
use aura_session::SessionManager;
use aura_workspace::WorkspacePaths;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::compressor::{SUMMARIZE_INSTRUCTION, parse_summary_response};
use crate::error::ContextError;

/// Result of one chat call inside the background-summary flow. Wraps
/// the sanitized [`LlmResponse`] with the trace span id + post-pricing
/// cost the call billed — `run_background_summary` writes both into
/// `session_summaries` so per-pass telemetry is queryable without
/// re-reading the trace store.
///
/// Same shape the inline path's `CompressionRunner::run` returns; the
/// inline closure discards everything but `response`.
pub struct SummaryChatRun {
    pub response: LlmResponse,
    pub span_id: String,
    pub cost_micros: i64,
}

pub type BackgroundSummaryFuture =
    Pin<Box<dyn Future<Output = std::result::Result<SummaryChatRun, ContextError>> + Send>>;

/// One-shot chat invocation handed to [`run_background_summary`].
/// Invoked at most once per pass, after the prior-summary read and
/// prompt build have produced the [`ChatRequest`].
pub type BackgroundSummaryCallback = Box<dyn FnOnce(ChatRequest) -> BackgroundSummaryFuture + Send>;

/// Required inputs for one [`run_background_summary`] pass. `model_id`
/// is the LLM the chat callback will hit — recorded against
/// `session_summaries` for telemetry.
pub struct BackgroundSummaryConfig {
    pub workspace: Arc<WorkspacePaths>,
    pub sessions: Arc<SessionManager>,
    pub parent_session_id: SessionId,
    pub up_to_ordinal: i64,
    pub model_id: String,
}

/// Result of one [`run_background_summary`] pass. Carried back as the
/// `JobOutput::Structured.value` for the maintenance job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundSummaryOutcome {
    pub cursor: i64,
    pub model_id: String,
    pub span_id: String,
    pub cost_micros: i64,
}

/// Trailing instruction appended to the parent's transcript before the
/// LLM call. Wraps the shared [`SUMMARIZE_INSTRUCTION`] (so the
/// analysis/summary contract stays in lockstep with the inline path)
/// with a prior-summary preamble for terminology continuity and a
/// Pattern B size target.
fn build_summary_prompt(prior_summary: Option<&str>) -> String {
    let prior = match prior_summary {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => "(none — this is the first pass)",
    };

    format!(
        "CONTEXT: A previous summary of part of this conversation exists and is provided \
below for terminology and structural consistency. The conversation transcript above is \
the authoritative source — re-derive every fact from it. Only use the prior summary as \
a scaffold to keep names, file paths, and concept labels stable across passes.\n\n\
PRIOR SUMMARY:\n{prior}\n\n---\n\n\
{SUMMARIZE_INSTRUCTION}\n\n\
SIZE TARGET: aim for ~8-12K tokens. Grow when genuinely more substance has \
accumulated; do not pad."
    )
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

async fn load_parent_transcript_up_to(
    sessions: &SessionManager,
    parent_id: &SessionId,
    up_to_ordinal: i64,
) -> Result<Vec<ChatMessage>, ContextError> {
    // SQL pushes both the supersede filter and the ordinal upper
    // bound, so neither superseded rows nor the post-snapshot tail
    // cross the wire.
    let out = sessions
        .load_active_session_messages_up_to(parent_id, up_to_ordinal)
        .await
        .map_err(|e| {
            ContextError::Compression(format!(
                "load_active_session_messages_up_to({parent_id}): {e}"
            ))
        })?;
    debug!(
        parent_session_id = %parent_id,
        up_to_ordinal,
        loaded = out.len(),
        "background summary: loaded parent transcript slice"
    );
    Ok(out)
}

/// Execute one background-summary pass. Returns the structured outcome
/// on success; on failure, increments the parent's `error_count` and
/// returns the underlying error.
///
/// The chat callback is the only agent-layer-coupled piece — it bundles
/// the LLM client + cost + trace recording machinery the inline path
/// also uses. Everything else (transcript load, prior-summary read,
/// prompt build, parse, atomic write, metadata record) is owned here.
pub async fn run_background_summary(
    config: BackgroundSummaryConfig,
    chat: BackgroundSummaryCallback,
) -> Result<BackgroundSummaryOutcome, ContextError> {
    let BackgroundSummaryConfig {
        workspace,
        sessions,
        parent_session_id,
        up_to_ordinal,
        model_id,
    } = config;

    // `up_to_ordinal` pins the snapshot the trigger fired against so
    // concurrent appends to the parent's transcript don't bleed into
    // this pass' input.
    let parent_messages =
        match load_parent_transcript_up_to(&sessions, &parent_session_id, up_to_ordinal).await {
            Ok(m) => m,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&parent_session_id, &model_id, "", Utc::now())
                    .await;
                return Err(e);
            }
        };
    if parent_messages.is_empty() {
        warn!(
            parent_session_id = %parent_session_id,
            "background summary: parent transcript is empty after load — skipping pass"
        );
        return Ok(BackgroundSummaryOutcome {
            cursor: up_to_ordinal,
            model_id,
            span_id: String::new(),
            cost_micros: 0,
        });
    }

    let prior_summary = match read_existing_summary(&workspace, &parent_session_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                parent_session_id = %parent_session_id,
                error = %e,
                "background summary: failed to read existing summary; treating as first-pass"
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

    let SummaryChatRun {
        response,
        span_id,
        cost_micros,
    } = match chat(request).await {
        Ok(run) => run,
        Err(e) => {
            let _ = sessions
                .record_summary_failure(&parent_session_id, &model_id, "", Utc::now())
                .await;
            return Err(e);
        }
    };

    let summary_text = parse_summary_response(&response.content)
        .ok_or_else(|| ContextError::Compression("summary response empty after parsing".into()))?;

    if let Err(e) = atomic_write_summary(&workspace, &parent_session_id, &summary_text).await {
        warn!(
            parent_session_id = %parent_session_id,
            error = %e,
            "background summary: atomic file write failed; metadata not updated"
        );
        let _ = sessions
            .record_summary_failure(&parent_session_id, &model_id, &span_id, Utc::now())
            .await;
        return Err(ContextError::Compression(format!(
            "atomic write failed: {e}"
        )));
    }

    // On metadata failure the file lands but the row doesn't —
    // an FS orphan that the next successful pass overwrites or
    // the startup reaper cleans up. Return success regardless.
    if let Err(e) = sessions
        .record_summary_success(
            &parent_session_id,
            up_to_ordinal,
            cost_micros,
            &model_id,
            &span_id,
            Utc::now(),
        )
        .await
    {
        warn!(
            parent_session_id = %parent_session_id,
            error = %e,
            "background summary: metadata write failed; FS orphan possible"
        );
    }

    info!(
        parent_session_id = %parent_session_id,
        cursor = up_to_ordinal,
        cost_micros,
        "background summary: pass succeeded"
    );

    Ok(BackgroundSummaryOutcome {
        cursor: up_to_ordinal,
        model_id,
        span_id,
        cost_micros,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_summary_prompt_includes_size_target_and_prior_marker() {
        let prompt = build_summary_prompt(None);
        assert!(prompt.contains("SIZE TARGET"));
        assert!(prompt.contains("(none — this is the first pass)"));

        let prompt2 = build_summary_prompt(Some("OLD SUMMARY BODY"));
        assert!(prompt2.contains("OLD SUMMARY BODY"));
        assert!(!prompt2.contains("(none —"));
    }
}
