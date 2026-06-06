//! Framing for the autonomous `SubagentNotification` turn — the synthetic
//! prompt the parent session runs when one or more background subagents
//! finish. Built here (pure) so the framing lives with the rest of the
//! prompt-injection text; the agent actor appends the result **in-memory
//! only** (rebuilt from the durable `pending_subagent_results` buffer on
//! every retry) and never persists it.

use aura_model::{ContentBlock, PendingSubagentResult, SubagentExitStatus};

/// Opening framing for a `SubagentNotification` turn's content. Lives in
/// per-turn content (never the system prompt) so the prompt-cache prefix
/// is identical to a normal main-path turn. Cron-style: report proactively.
const SUBAGENT_NOTIFICATION_FRAMING: &str = "[background subagent task(s) finished since your last turn — report the outcome to the user as a fresh, proactive message.]";

/// Per-result element of the nested `<subagent_results>` block. Metadata
/// rides as attributes; `task` / `output` are child elements so multi-line
/// free text with quotes needs no attribute escaping.
const SUBAGENT_RESULT_TEMPLATE: &str = r#"  <result handle="{{handle}}" type="{{type}}" status="{{status}}">
    <task>{{task}}</task>
    <output>{{output}}</output>
    <child_session>{{child_session}}</child_session>
  </result>
"#;

/// Render pending background-subagent results into nested-XML content for one
/// `SubagentNotification` turn. Pure — the caller owns the buffer so it can
/// restore the results if the turn fails. The framing rides in this per-turn
/// content (never the system prompt) so the prompt-cache prefix stays
/// identical to a normal main-path turn.
pub fn build_notification_content(pending: &[PendingSubagentResult]) -> Vec<ContentBlock> {
    let mut xml = String::from(SUBAGENT_NOTIFICATION_FRAMING);
    xml.push_str("\n\n<subagent_results>\n");
    for p in pending {
        xml.push_str(
            &SUBAGENT_RESULT_TEMPLATE
                .replace("{{handle}}", &xml_escape(&p.handle_id))
                .replace("{{type}}", &xml_escape(&p.subagent_type))
                .replace("{{status}}", pending_status_label(&p.status))
                .replace("{{task}}", &xml_escape(&p.task_summary))
                .replace(
                    "{{output}}",
                    &xml_escape(&truncate_for_notice(&p.final_text)),
                )
                .replace(
                    "{{child_session}}",
                    &xml_escape(p.child_session_id.as_ref()),
                ),
        );
    }
    xml.push_str("</subagent_results>");
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

/// Cap a result's free text so one chatty subagent can't blow the notification
/// turn's budget; the full text stays in the child session transcript.
fn truncate_for_notice(text: &str) -> String {
    const MAX: usize = 1024;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX).collect();
    format!("{truncated}… [truncated; full text in child session transcript]")
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
    fn build_notification_frames_and_escapes() {
        let pending = vec![PendingSubagentResult {
            handle_id: "h1".into(),
            subagent_type: "claude".into(),
            task_summary: "do <stuff>".into(),
            child_session_id: aura_model::SessionId::from("child-1"),
            final_text: "result & more".into(),
            status: SubagentExitStatus::Completed,
        }];
        let blocks = build_notification_content(&pending);
        let ContentBlock::Text(xml) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(xml.starts_with(SUBAGENT_NOTIFICATION_FRAMING));
        assert!(xml.contains("<subagent_results>"));
        assert!(xml.contains("status=\"completed\""));
        // Free text is XML-escaped.
        assert!(xml.contains("do &lt;stuff&gt;"));
        assert!(xml.contains("result &amp; more"));
        assert!(xml.contains("<child_session>child-1</child_session>"));
    }
}
