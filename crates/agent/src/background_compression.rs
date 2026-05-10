//! Async per-session background-compression handler invoked by the
//! maintenance actor's `AgentMessage::SystemTrigger` path. The actual
//! LLM call goes through [`crate::compression::CompressionRunner`],
//! the same helper the inline (`maybe_compress`) path uses; this
//! module owns the pre/post bookkeeping (transcript load, prompt
//! build, atomic file write, metadata update, orphan reaper).
//! See `docs/background-compression.md`.

use std::path::PathBuf;
use std::sync::Arc;

use aura_context::{SUMMARIZE_INSTRUCTION, parse_summary_response};
use aura_llm::{ChatRequest, GuardedLlm, ModelInfo};
use aura_model::{ChatMessage, ContentBlock, JobId, Role, SessionId};
use aura_session::SessionManager;
use aura_workspace::WorkspacePaths;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use anyhow::Context as _;

use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::trace::SpanRecorder;

/// Synthetic `model_id` recorded against `session_summaries.error_count`
/// when the orphan reaper bumps a parent's failure count for a
/// crashed-mid-pass maintenance session.
const ORPHAN_REAP_MODEL_TAG: &str = "orphan-reap";

/// Payload carried by `JobInput::System { reason: BackgroundCompression, payload }`.
/// The parent's agent loop builds this at trigger spawn time;
/// the maintenance session's actor parses it via `serde_json::from_value`
/// before invoking the refresher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundCompressionPayload {
    pub parent_session_id: SessionId,
    /// Highest `session_messages.ordinal` to include in this pass'
    /// input. Pinned at trigger time so concurrent appends to the
    /// parent don't bleed in mid-pass.
    pub up_to_ordinal: i64,
}

/// Result of one refresh pass; carried back as the
/// `JobOutput::Structured.value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BackgroundCompressionOutcome {
    pub cursor: i64,
    pub model_id: String,
    pub span_id: String,
    pub cost_micros: i64,
}

/// Trailing instruction appended to the parent's transcript before
/// the LLM call. Wraps the shared `SUMMARIZE_INSTRUCTION` (so the
/// analysis/summary contract stays in lockstep with `Summarize`'s
/// inline path) with a prior-summary preamble for terminology
/// continuity and a Pattern B size target.
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

/// Request emitted by `AgentLoop`'s parent-side trigger gate and
/// consumed by `Router`'s `system_trigger_rx` arm. Replaces the old
/// `MaintenanceSpawner` trait: the gate just sends a value on an
/// `mpsc::Sender<SystemSpawnRequest>`; the router does the
/// session-create + actor-spawn + mailbox-dispatch.
///
/// `parent_actor_token` is the parent actor's lifetime token. The
/// router uses it as the new maintenance actor's `parent_token`, so
/// the maintenance child's `actor_token` derives as a grandchild —
/// cancelling the parent automatically cascades into the child via
/// the `tokio_util` token tree, with no explicit `Shutdown` mailbox
/// dance.
#[derive(Debug)]
pub enum SystemSpawnRequest {
    BackgroundCompression {
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_actor_token: CancellationToken,
        payload: BackgroundCompressionPayload,
    },
}

/// Inputs for one refresh pass — all the agent-layer machinery the
/// dedicated handler needs without dragging the entire `AgentLoop`
/// surface in.
pub(crate) struct BackgroundCompressionRunner {
    pub llm_client: Arc<GuardedLlm>,
    pub security_gateway: Arc<SecurityGateway>,
    pub cost_manager: Arc<CostManager>,
    pub sessions: Arc<SessionManager>,
    pub workspace_paths: Arc<WorkspacePaths>,
    pub recorder: Arc<SpanRecorder>,
    pub model_info: ModelInfo,
    pub maintenance_session_id: SessionId,
    pub maintenance_user_id: String,
    pub job_id: JobId,
    pub cancel_token: CancellationToken,
}

impl BackgroundCompressionRunner {
    /// Execute one summary refresh pass. Returns the structured
    /// outcome on success; on failure, increments the parent's
    /// `error_count` and returns the underlying error.
    pub async fn run(
        self,
        payload: BackgroundCompressionPayload,
    ) -> anyhow::Result<BackgroundCompressionOutcome> {
        let BackgroundCompressionRunner {
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
            return Ok(BackgroundCompressionOutcome {
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

        let runner = crate::compression::CompressionRunner {
            llm_client,
            recorder,
            cost_manager,
            security_gateway,
            job_id,
            user_id: maintenance_user_id,
            session_id: maintenance_session_id,
            model_info: model_info.clone(),
            cancel_token,
        };

        let crate::compression::CompressionRun {
            response,
            span_id,
            cost_micros,
        } = match runner.run(request).await {
            Ok(run) => run,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&parent_id, &model_info.id, "", Utc::now())
                    .await;
                return Err(anyhow::anyhow!(e));
            }
        };

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

        Ok(BackgroundCompressionOutcome {
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
pub async fn reap_maintenance_orphans(
    sessions: &SessionManager,
    workspace_paths: &aura_workspace::WorkspacePaths,
) {
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
        let p = BackgroundCompressionPayload {
            parent_session_id: SessionId::from("user-1"),
            up_to_ordinal: 42,
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: BackgroundCompressionPayload = serde_json::from_value(v).unwrap();
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
