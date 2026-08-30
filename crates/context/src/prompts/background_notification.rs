//! Framing for the autonomous background-notification turn — the synthetic prompt the
//! parent session runs when one or more background jobs (detached subagents
//! and detached `Bash` commands) finish. Built here (pure) so the framing
//! lives with the rest of the prompt-injection text; the agent actor persists
//! the result as a hidden agent-context row and records it on the session's
//! notification ledger, which drives delivery retries.

use std::path::{Path, PathBuf};

use baybo_model::{BackgroundJobKind, ContentBlock, PendingBackgroundResult, SubagentExitStatus};
use baybo_workspace::WorkspacePaths;

/// Assistant-reply lead sent before the parent starts analysing a finished
/// background batch. Carries no result content (see [`build_completion_reply`]);
/// `{{count_noun}}` is pluralised per batch size ("result" / "results").
const BACKGROUND_COMPLETION_REPLY_LEAD: &str =
    "Background work has finished. I'm reviewing the {{count_noun}} now.";

/// Opening framing for a notification turn's content. Lives in per-turn
/// content (never the system prompt) so the prompt-cache prefix is identical
/// to a normal main-path turn.
const BACKGROUND_NOTIFICATION_FRAMING: &str = "[background task(s) finished since your last turn. A brief completion acknowledgement has already been sent to the user. Analyze the results now and report the useful outcome as the next fresh, proactive message; do not repeat the acknowledgement.]";

/// Per-result body budget, matching what the SAME report would have kept had
/// the job finished inside its foreground wait and come back as a tool result.
/// Before this the two paths disagreed by ~30x, so a subagent that crossed the
/// wait and converted to background had its report gutted while an identical
/// one finishing a second earlier arrived whole.
const MAX_RESULT_BYTES: usize = baybo_model::MAX_TOOL_OUTPUT_BYTES;

/// Ceiling on one notification turn's combined result bodies. The buffer holds
/// up to 64 terminal results and drains them into a single prompt, so a
/// per-result budget alone would let a wide fan-out land a multi-megabyte
/// turn. Split evenly; at the batch sizes that actually occur (a handful)
/// every result still gets the full per-result budget.
const MAX_BATCH_BYTES: usize = 4 * MAX_RESULT_BYTES;

/// Per-result element of the nested `<background_results>` block. Metadata
/// rides as attributes; `task` / `output` (and the kind-specific `detail`)
/// are child elements so multi-line free text with quotes needs no attribute
/// escaping.
const BACKGROUND_RESULT_TEMPLATE: &str = r#"  <result handle="{{handle}}" type="{{type}}" status="{{status}}">
    <task>{{task}}</task>
    <output>{{output}}</output>
{{detail}}  </result>
"#;

/// Request-time cue that gives a notification-turn request a user-role tail.
/// It is load-bearing, not decoration: a cancelled attempt's salvage leaves an
/// assistant row at the transcript tail, and a request ending on an assistant
/// message is provider *prefill* — rejected outright by Anthropic with
/// extended thinking on — so the retry needs a user-side tail; it also
/// un-buries the prompt from behind the failed attempt's partial rows, and it
/// makes a blank retry reply a genuine "nothing to add" judgment instead of
/// an "I already answered above" artifact.
///
/// Never persisted: it is applied as a request-time suffix
/// ([`ContextManager::set_notification_cue`]) only while the transcript tail is
/// an assistant row, so it is recomputed per request rather than carrying an
/// attempt number — a persisted, attempt-keyed cue was a no-op on the exact
/// crash-replay it had to survive (the counter only advances on an observed
/// failure) and left permanent rows in the append-only log.
const RETRY_CUE: &str = "[the user has received only the completion acknowledgement; no complete report for the background results above has reached them yet — produce the complete report now.]";

/// Build the user-facing acknowledgement that precedes the streamed analysis
/// turn. It is a bland "work finished, reviewing now" notice with **no result
/// content**: a finished turn's raw output stays LLM-only (it rides
/// [`build_notification_content`] as a hidden `agent_context` row), so the user
/// learns the actual outcome solely from the parent's analysed report, never
/// from the unprocessed result body.
///
/// Returns plain text, not content blocks, because the actor persists this on
/// the **control-event** plane (`session_control_events`) rather than in
/// `session_messages` — see the "Buffer-to-delivery durability boundary"
/// section of `docs/background-notifications.md` for why it cannot be a
/// transcript row.
pub fn build_completion_reply(pending: &[PendingBackgroundResult]) -> String {
    let count_noun = if pending.len() > 1 {
        "results"
    } else {
        "result"
    };
    BACKGROUND_COMPLETION_REPLY_LEAD.replace("{{count_noun}}", count_noun)
}

/// The request-time retry cue (see [`RETRY_CUE`]). Stateless — the same for
/// every attempt, because the decision to apply it is made per request from
/// the transcript tail, not from an attempt counter.
pub fn build_retry_cue() -> Vec<ContentBlock> {
    vec![ContentBlock::Text(RETRY_CUE.to_string())]
}

/// Render pending background-turn results into nested-XML content for one
/// notification turn. Pure — the actor freezes the rendered content on the
/// notification ledger, so a retry re-runs against exactly this prompt. The
/// framing rides in this per-turn content (never the system prompt) so the
/// prompt-cache prefix stays identical to a normal main-path turn.
pub fn build_notification_content(
    pending: &[PendingBackgroundResult],
    workspace: &WorkspacePaths,
) -> Vec<ContentBlock> {
    let budget = result_budget(pending.len());
    let mut xml = String::from(BACKGROUND_NOTIFICATION_FRAMING);
    xml.push_str("\n\n<background_results>\n");
    for p in pending {
        let (type_attr, full_text, detail) = match &p.kind {
            BackgroundJobKind::Subagent {
                child_session_id,
                subagent_type,
            } => {
                // The child's transcript is served virtually out of the store
                // (see `SessionTranscriptReader`), so this address is `Read`-able
                // even though nothing is written there. Naming it is what makes
                // the truncation notice below actionable: the id alone only
                // suggests a resume, which re-runs an LLM to re-emit text that
                // is already sitting in the database.
                let transcript = workspace.session_log_file(child_session_id.as_ref());
                (
                    subagent_type.clone(),
                    transcript.clone(),
                    format!(
                        "    <child_session>{}</child_session>\n    <transcript_file>{}</transcript_file>\n",
                        xml_escape(child_session_id.as_ref()),
                        xml_escape(&transcript.display().to_string()),
                    ),
                )
            }
            BackgroundJobKind::Command {
                exit_code,
                output_path,
                ..
            } => (
                "command".to_string(),
                PathBuf::from(output_path),
                format!(
                    "    <exit_code>{}</exit_code>\n    <output_file>{}</output_file>\n",
                    exit_code,
                    xml_escape(output_path)
                ),
            ),
        };
        xml.push_str(
            &BACKGROUND_RESULT_TEMPLATE
                .replace("{{handle}}", &xml_escape(&p.handle_id))
                .replace("{{type}}", &xml_escape(&type_attr))
                .replace("{{status}}", pending_status_label(&p.status))
                .replace("{{task}}", &xml_escape(&p.label))
                .replace(
                    "{{output}}",
                    &xml_escape(&truncate_for_notice(&p.summary_text, budget, &full_text)),
                )
                .replace("{{detail}}", &detail),
        );
    }
    xml.push_str("</background_results>");
    vec![ContentBlock::Text(xml)]
}

/// Bytes each result in a batch of `count` may spend on its body.
fn result_budget(count: usize) -> usize {
    MAX_RESULT_BYTES.min(MAX_BATCH_BYTES / count.max(1))
}

fn pending_status_label(status: &SubagentExitStatus) -> &'static str {
    match status {
        SubagentExitStatus::Completed => "completed",
        SubagentExitStatus::Cancelled => "cancelled",
        SubagentExitStatus::Failed { .. } => "failed",
        SubagentExitStatus::Timeout => "timeout",
        SubagentExitStatus::ForegroundWaitElapsed => "killed",
    }
}

/// Cap a result's free text at `budget` bytes so one chatty batch can't blow
/// the notification turn, and point the model at `full_text` — a `Read`-able
/// absolute path — for the rest.
///
/// The path is the whole point. The notice used to say "full text in the
/// turn's transcript / output file", which named no path and, for a subagent,
/// named a file that does not exist; the only affordance beside it was a bare
/// child-session id, so a parent that noticed the cut re-spawned the child to
/// re-dictate text that was already in the store, and paid an LLM round-trip
/// to fail at it.
fn truncate_for_notice(text: &str, budget: usize, full_text: &Path) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let cut = text.floor_char_boundary(budget);
    format!(
        "{}… [truncated: {} of {} bytes shown. The FULL text is at {} — `Read` that absolute path (with `offset`/`limit` to page) instead of re-running or resuming the job.]",
        &text[..cut],
        cut,
        text.len(),
        full_text.display(),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_status_label_covers_every_exit_status() {
        use SubagentExitStatus as S;
        assert_eq!(pending_status_label(&S::Completed), "completed");
        assert_eq!(pending_status_label(&S::Cancelled), "cancelled");
        assert_eq!(
            pending_status_label(&S::Failed { reason: "x".into() }),
            "failed"
        );
        assert_eq!(pending_status_label(&S::Timeout), "timeout");
        assert_eq!(pending_status_label(&S::ForegroundWaitElapsed), "killed");
    }

    fn workspace() -> WorkspacePaths {
        WorkspacePaths::new(PathBuf::from("/ws"))
    }

    #[test]
    fn truncate_for_notice_appends_marker_and_the_recovery_path_when_over_cap() {
        let long = "a".repeat(MAX_RESULT_BYTES * 2);
        let path = Path::new("/ws/logs/sessions/child-1.jsonl");
        let out = truncate_for_notice(&long, MAX_RESULT_BYTES, path);
        assert!(out.contains("truncated"));
        // A cut that does not say where the rest is leaves the parent to
        // re-run work that is already in the store.
        assert!(
            out.contains("/ws/logs/sessions/child-1.jsonl"),
            "truncation marker must name the readable path: {out}"
        );
        assert_eq!(
            truncate_for_notice("hello", MAX_RESULT_BYTES, path),
            "hello"
        );
    }

    /// A subagent report that a FOREGROUND return would have delivered whole
    /// must survive the background notification whole too.
    ///
    /// This is the regression the incident turned on: a report crossed the
    /// 120s foreground wait, converted to background, and lost 85% of itself
    /// to a 1024-char cap — the same bytes a tool result would have kept under
    /// `MAX_TOOL_OUTPUT_BYTES`.
    #[test]
    fn a_report_that_fits_a_tool_result_is_not_cut_by_the_notification() {
        let report = "彤程新材".repeat(1024); // ~12 KiB, the incident's size
        assert!(report.len() <= baybo_model::MAX_TOOL_OUTPUT_BYTES);
        let pending = vec![PendingBackgroundResult::subagent(
            "h1",
            "explorer",
            "peer financials",
            baybo_model::SessionId::from("child-1"),
            report.clone(),
            SubagentExitStatus::Completed,
        )];
        let blocks = build_notification_content(&pending, &workspace());
        let ContentBlock::Text(xml) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(xml.contains(&report), "report was cut: {} bytes", xml.len());
        assert!(!xml.contains("truncated"));
    }

    /// The per-result budget shrinks so a wide fan-out cannot land a
    /// multi-megabyte turn, but a handful of results each keep the full one.
    #[test]
    fn batch_budget_splits_across_results_but_spares_small_batches() {
        assert_eq!(result_budget(1), MAX_RESULT_BYTES);
        assert_eq!(result_budget(4), MAX_RESULT_BYTES);
        assert_eq!(result_budget(64), MAX_BATCH_BYTES / 64);
        assert!(result_budget(64) * 64 <= MAX_BATCH_BYTES);
        // `count` is the batch length, which the caller can hand in as 0.
        assert_eq!(result_budget(0), MAX_RESULT_BYTES);
    }

    /// A multi-byte character straddling the budget must not panic or emit
    /// a broken code point — the incident's report was Chinese throughout.
    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        let text = "彤".repeat(64); // 3 bytes each
        let out = truncate_for_notice(&text, 100, Path::new("/ws/x.jsonl"));
        assert!(out.starts_with(&"彤".repeat(33)));
        assert!(!out.starts_with(&"彤".repeat(34)));
    }

    #[test]
    fn build_notification_frames_and_escapes_subagent() {
        let pending = vec![PendingBackgroundResult::subagent(
            "h1",
            "claude",
            "do <stuff>",
            baybo_model::SessionId::from("child-1"),
            "result & more",
            SubagentExitStatus::Completed,
        )];
        let blocks = build_notification_content(&pending, &workspace());
        let ContentBlock::Text(xml) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(xml.starts_with(BACKGROUND_NOTIFICATION_FRAMING));
        assert!(xml.contains("<background_results>"));
        assert!(xml.contains("status=\"completed\""));
        assert!(xml.contains("type=\"claude\""));
        // Free text is XML-escaped.
        assert!(xml.contains("do &lt;stuff&gt;"));
        assert!(xml.contains("result &amp; more"));
        assert!(xml.contains("<child_session>child-1</child_session>"));
        // The subagent arm must name a readable address, not just an id —
        // mirroring `<output_file>` on the command arm below. An id alone
        // only affords a resume, which re-runs an LLM to re-emit stored text.
        assert!(
            xml.contains("<transcript_file>/ws/logs/sessions/child-1.jsonl</transcript_file>"),
            "missing transcript pointer: {xml}"
        );
    }

    #[test]
    fn build_notification_renders_command_kind() {
        let pending = vec![PendingBackgroundResult::command(
            "bg-cmd-1",
            "cargo build --release",
            0,
            "logs/background/bg-cmd-1.log",
            "Compiling…\nFinished",
            SubagentExitStatus::Completed,
        )];
        let blocks = build_notification_content(&pending, &workspace());
        let ContentBlock::Text(xml) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(xml.contains("type=\"command\""));
        assert!(xml.contains("<exit_code>0</exit_code>"));
        assert!(xml.contains("<output_file>logs/background/bg-cmd-1.log</output_file>"));
        assert!(xml.contains("cargo build --release"));
    }

    #[test]
    fn completion_reply_is_bland_acknowledgement_without_result() {
        let pending = vec![PendingBackgroundResult::subagent(
            "h1",
            "explorer",
            "find the file",
            baybo_model::SessionId::from("child-1"),
            "found it at src/lib.rs",
            SubagentExitStatus::Completed,
        )];
        // The raw result is LLM-only (it rides build_notification_content), so
        // the user-facing acknowledgement is exactly the lead — no result body.
        assert_eq!(
            build_completion_reply(&pending),
            BACKGROUND_COMPLETION_REPLY_LEAD.replace("{{count_noun}}", "result")
        );
    }

    #[test]
    fn completion_reply_omits_every_result_body_for_a_batch() {
        let pending = vec![
            PendingBackgroundResult::subagent(
                "h1",
                "explorer",
                "first task",
                baybo_model::SessionId::from("child-1"),
                "first summary",
                SubagentExitStatus::Completed,
            ),
            PendingBackgroundResult::subagent(
                "h2",
                "explorer",
                "second task",
                baybo_model::SessionId::from("child-2"),
                "second summary",
                SubagentExitStatus::Completed,
            ),
        ];
        let reply = build_completion_reply(&pending);
        assert_eq!(
            reply,
            BACKGROUND_COMPLETION_REPLY_LEAD.replace("{{count_noun}}", "results")
        );
        // Neither a result body nor a task label may leak into the batch
        // acknowledgement.
        for leaked in [
            "first summary",
            "second summary",
            "first task",
            "second task",
        ] {
            assert!(
                !reply.contains(leaked),
                "acknowledgement leaked turn content: {leaked:?} in {reply:?}"
            );
        }
    }
}
