//! Wire format for the prompt a cron job fires.
//!
//! A fire is delivered to the model as a *user* turn, so without explicit
//! framing the model reads the bare prompt as something the user just
//! said — a job created to "say hi in a minute" stores the prompt `hi`,
//! fires it verbatim, and the model greets back instead of performing the
//! send. The framing built here makes three things unambiguous to the
//! model: (1) this is a scheduled fire, not a live user message; (2) the
//! prompt is an instruction to carry out now and report back to the user;
//! (3) the `cron:<id>` tag is diagnostic-only and must never surface in
//! the reply.
//!
//! [`frame_cron_prompt`] is the single synthesis seam (the agent actor's
//! cron dispatch appends it via `ContextManager`); [`original_cron_prompt`]
//! reverses it so operator surfaces (the admin chat panel) can render the
//! instruction as configured rather than the framing boilerplate.

/// Framing paragraph inserted between the routing tag and the original
/// instruction. Must not itself contain [`INSTRUCTION_LABEL`], so
/// [`original_cron_prompt`] can split on the label's first occurrence.
const FRAMING_BODY: &str = r#"The text below was scheduled earlier and is firing automatically right now — it is NOT a new message the user just sent, so do not respond to it as if the user is speaking to you. Instead, carry out the instruction now and send the result to the user as a fresh, proactive message. The tag above is only for your own diagnostics if you need to look up or manage this job; never repeat that id in your reply to the user."#;

/// Label that introduces the original instruction. Distinctive enough
/// that it does not occur in [`FRAMING_BODY`]; [`original_cron_prompt`]
/// splits on its first occurrence to recover the prompt exactly as
/// configured.
const INSTRUCTION_LABEL: &str = "Scheduled instruction to perform now:";

/// Leading routing tag every framed cron prompt carries: `[cron:<id>] …`.
/// Kept first so the LLM's own diagnostics and trace tooling can read the job
/// id; operator surfaces identify the cron row by its `MessageSource::Cron`
/// provenance, not by sniffing this tag.
const CRON_TAG_PREFIX: &str = "[cron:";

/// Build the fire-time content for a cron job. The routing tag
/// (`[cron:job_id]`) stays first so trace tooling and legacy-row recovery
/// can still locate it; the framing and the original `prompt` follow.
pub fn frame_cron_prompt(job_id: &str, prompt: &str) -> String {
    format!("{CRON_TAG_PREFIX}{job_id}] {FRAMING_BODY}\n\n{INSTRUCTION_LABEL}\n{prompt}")
}

/// Header of a one-shot fire's result, delivered into the conversation that
/// scheduled it. `{{title}}` is the job's display title.
///
/// The framing is baked into the persisted row rather than applied wire-side
/// (as the user/interjection envelopes are): this row is `Role::Assistant`, so
/// the model reads it back as something it already said — and it must read the
/// *same* bytes the user sees, or the two would disagree about what was
/// reported. It also means a boot-time re-delivery reproduces the row exactly.
///
/// English, like every other prompt in the tree: the model reads this back as
/// its own words, so a header in a fixed non-English language would put words
/// in its mouth it never chose. The *body* — the fire's own reply — carries
/// whatever language the job's prompt asked for, so a Chinese reminder still
/// reads as Chinese.
const NOTIFICATION_HEADER_SUCCESS: &str = r#"⏰ Scheduled task "{{title}}" ran:"#;
const NOTIFICATION_HEADER_FAILED: &str = r#"⏰ Scheduled task "{{title}}" failed:"#;

/// Body used when a fire completed but produced no text (a tool-only or empty
/// reply). The notification still lands — a scheduled task must never report
/// nothing — it just says so.
const NOTIFICATION_BLANK_BODY: &str = "It ran, but produced no output.";

/// Body used when a failed fire left no error detail to quote.
const NOTIFICATION_FAILURE_UNKNOWN: &str = "No error detail was recorded.";

/// The leading text block of a one-shot cron notification: a header naming the
/// job, followed by `body` (the fire's reply text, a blank-run fallback, or the
/// failure reason). The caller appends any non-text blocks of the fire's reply
/// (images, files) after this one, so nothing the fire produced is lost.
pub fn frame_cron_notification(title: &str, failed: bool, body: &str) -> String {
    let header = if failed {
        NOTIFICATION_HEADER_FAILED
    } else {
        NOTIFICATION_HEADER_SUCCESS
    }
    .replace("{{title}}", title);

    let body = body.trim();
    let body = if !body.is_empty() {
        body
    } else if failed {
        NOTIFICATION_FAILURE_UNKNOWN
    } else {
        NOTIFICATION_BLANK_BODY
    };
    format!("{header}\n\n{body}")
}

/// Recover the original instruction from synthesized cron content, for
/// operator-facing previews.
///
/// Handles both the framed format produced by [`frame_cron_prompt`]
/// (everything after the first [`INSTRUCTION_LABEL`]) and legacy rows
/// persisted as the bare `[cron:id] prompt` prefix (strip the tag). Falls
/// back to the content unchanged when neither shape matches.
pub fn original_cron_prompt(content: &str) -> &str {
    if let Some((_, rest)) = content.split_once(INSTRUCTION_LABEL) {
        return rest.trim_start();
    }
    if let Some(rest) = content.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[end + 1..].trim_start();
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_announces_scheduled_fire_and_hides_tag() {
        let framed = frame_cron_prompt("cj-1", "hi");
        assert!(framed.starts_with("[cron:cj-1] "), "{framed}");
        assert!(framed.contains("NOT a new message the user just sent"));
        assert!(framed.contains("never repeat that id"));
        assert!(framed.contains("hi"));
    }

    #[test]
    fn round_trips_to_original_prompt() {
        let framed = frame_cron_prompt("cj-1", "hi");
        assert_eq!(original_cron_prompt(&framed), "hi");
    }

    #[test]
    fn round_trips_multiline_prompt() {
        let prompt = "Summarise today's news.\n\nKeep it under 5 bullets.";
        let framed = frame_cron_prompt("cj-2", prompt);
        assert_eq!(original_cron_prompt(&framed), prompt);
    }

    #[test]
    fn recovers_prompt_that_contains_the_label() {
        let prompt = "Scheduled instruction to perform now: greet the user";
        let framed = frame_cron_prompt("cj-3", prompt);
        assert_eq!(original_cron_prompt(&framed), prompt);
    }

    #[test]
    fn strips_legacy_bare_tag_prefix() {
        assert_eq!(
            original_cron_prompt("[cron:cj-test] morning brief"),
            "morning brief"
        );
    }

    #[test]
    fn returns_untagged_content_unchanged() {
        assert_eq!(original_cron_prompt("just text"), "just text");
    }

    /// The header is English (it is the assistant's own voice, like every other
    /// prompt in the tree) while the body is whatever the fire actually
    /// produced — a job asked to remind the user in Chinese still reports in
    /// Chinese under an English header. Title and body are both passed through
    /// verbatim.
    #[test]
    fn notification_headers_name_the_job_and_carry_the_body() {
        let ok = frame_cron_notification("每日新闻", false, "今天的三条新闻…");
        assert!(
            ok.starts_with(r#"⏰ Scheduled task "每日新闻" ran:"#),
            "{ok}"
        );
        assert!(ok.ends_with("今天的三条新闻…"));

        let failed = frame_cron_notification("Daily news", true, "provider timed out");
        assert!(
            failed.starts_with(r#"⏰ Scheduled task "Daily news" failed:"#),
            "{failed}"
        );
        assert!(failed.ends_with("provider timed out"));
    }

    /// A fire that produced nothing still notifies — silence is the one
    /// outcome a scheduled reminder must never have.
    #[test]
    fn empty_body_falls_back_per_outcome() {
        let blank = frame_cron_notification("Reminder", false, "   ");
        assert!(blank.ends_with(NOTIFICATION_BLANK_BODY), "{blank}");

        let failed = frame_cron_notification("Reminder", true, "");
        assert!(failed.ends_with(NOTIFICATION_FAILURE_UNKNOWN), "{failed}");
    }
}
