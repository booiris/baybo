//! Framing for the autonomous notification turn — the synthetic prompt the
//! parent session runs when one or more background jobs (detached subagents
//! and detached `Bash` commands) finish. Built here (pure) so the framing
//! lives with the rest of the prompt-injection text; the agent actor persists
//! the result as a hidden agent-context row and records it on the session's
//! notification ledger, which drives delivery retries.

use baybo_model::{BackgroundJobKind, ContentBlock, PendingBackgroundResult, SubagentExitStatus};

/// Opening framing for a notification turn's content. Lives in per-turn
/// content (never the system prompt) so the prompt-cache prefix is identical
/// to a normal main-path turn. Cron-style: report proactively.
const BACKGROUND_NOTIFICATION_FRAMING: &str = "[background task(s) finished since your last turn — report the outcome to the user as a fresh, proactive message.]";

/// Per-result element of the nested `<background_results>` block. Metadata
/// rides as attributes; `task` / `output` (and the kind-specific `detail`)
/// are child elements so multi-line free text with quotes needs no attribute
/// escaping.
const BACKGROUND_RESULT_TEMPLATE: &str = r#"  <result handle="{{handle}}" type="{{type}}" status="{{status}}">
    <task>{{task}}</task>
    <output>{{output}}</output>
{{detail}}  </result>
"#;

/// Cue row appended (persisted) ahead of a notification-turn retry. It is
/// load-bearing, not decoration: a cancelled attempt's salvage leaves an
/// assistant row at the transcript tail, and a request ending on an assistant
/// message is provider *prefill* — rejected outright by Anthropic with
/// extended thinking on — so the retry needs a user-side tail; it also
/// un-buries the prompt from behind the failed attempt's partial rows, and it
/// makes a blank retry reply a genuine "nothing to add" judgment instead of
/// an "I already answered above" artifact.
const RETRY_CUE_TEMPLATE: &str = "[delivery attempt {{attempt}} for the background results above was interrupted before a reply reached the user — produce the complete report now.]";

/// Render the persisted retry-cue row for delivery attempt `attempt`
/// (1-based; attempt 1 is the original drain, so cues start at 2).
pub fn build_retry_cue(attempt: u32) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(
        RETRY_CUE_TEMPLATE.replace("{{attempt}}", &attempt.to_string()),
    )]
}

/// Render pending background-job results into nested-XML content for one
/// notification turn. Pure — the actor freezes the rendered content on the
/// notification ledger, so a retry re-runs against exactly this prompt. The
/// framing rides in this per-turn content (never the system prompt) so the
/// prompt-cache prefix stays identical to a normal main-path turn.
pub fn build_notification_content(pending: &[PendingBackgroundResult]) -> Vec<ContentBlock> {
    let mut xml = String::from(BACKGROUND_NOTIFICATION_FRAMING);
    xml.push_str("\n\n<background_results>\n");
    for p in pending {
        let (type_attr, detail) = match &p.kind {
            BackgroundJobKind::Subagent {
                child_session_id,
                subagent_type,
            } => (
                subagent_type.clone(),
                format!(
                    "    <child_session>{}</child_session>\n",
                    xml_escape(child_session_id.as_ref())
                ),
            ),
            BackgroundJobKind::Command {
                exit_code,
                output_path,
                ..
            } => (
                "command".to_string(),
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
                    &xml_escape(&truncate_for_notice(&p.summary_text)),
                )
                .replace("{{detail}}", &detail),
        );
    }
    xml.push_str("</background_results>");
    vec![ContentBlock::Text(xml)]
}

fn pending_status_label(status: &SubagentExitStatus) -> &'static str {
    match status {
        SubagentExitStatus::Completed => "completed",
        SubagentExitStatus::Cancelled => "cancelled",
        SubagentExitStatus::Failed { .. } => "failed",
        SubagentExitStatus::Timeout => "timeout",
    }
}

/// Cap a result's free text so one chatty job can't blow the notification
/// turn's budget; the full text stays in the child session transcript (for a
/// subagent) or the job's output file (for a command).
fn truncate_for_notice(text: &str) -> String {
    const MAX: usize = 1024;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX).collect();
    format!("{truncated}… [truncated; full text in the job's transcript / output file]")
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
    }

    #[test]
    fn truncate_for_notice_appends_marker_when_over_cap() {
        let long = "a".repeat(2000);
        let out = truncate_for_notice(&long);
        assert!(
            out.len() > 1024,
            "marker must be appended on overflow: {out:?}"
        );
        assert!(out.contains("truncated"));
        let short = "hello";
        assert_eq!(truncate_for_notice(short), "hello");
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
        let blocks = build_notification_content(&pending);
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
        let blocks = build_notification_content(&pending);
        let ContentBlock::Text(xml) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(xml.contains("type=\"command\""));
        assert!(xml.contains("<exit_code>0</exit_code>"));
        assert!(xml.contains("<output_file>logs/background/bg-cmd-1.log</output_file>"));
        assert!(xml.contains("cargo build --release"));
    }
}
