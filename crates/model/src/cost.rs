use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{JobId, MicroUsd, SessionId, SpanId};

/// Token prefix for a [`CallReason::Tool`] so its tool name rides inside
/// the single flat `reason` token (`tool:WebFetch`) — one column, and a
/// per-tool analytics breakdown falls out for free.
const TOOL_TOKEN_PREFIX: &str = "tool:";

/// Why an LLM call was made — the call's *purpose*, orthogonal to who it
/// is billed to. Lets `cost_records` be grouped by reason directly
/// (`GROUP BY reason`) instead of inferring it from the span's `StepKind`
/// or, for platform spend, from a `system:<component>` session id.
///
/// Serializes to a single flat token (see [`CallReason::to_token`]) in
/// JSON and in the `cost_records.reason` column — `Tool` keeps its tool
/// name inline rather than degrading to an object, so every reason is one
/// groupable string.
///
/// Every billed call carries one via [`Attribution`](../../baybo_llm/billed/struct.Attribution.html);
/// new reasons are added as new call sites appear.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum CallReason {
    /// Primary agent turn answering a user (or cron) message. Also used
    /// for a nested subagent's external-CLI token accounting — that spend
    /// is the agent doing the task, distinguished by its own session id.
    #[default]
    Chat,
    /// An in-tool LLM sub-call (e.g. WebFetch's prompt-driven extraction),
    /// bound to the running tool's span. Carries the tool's registered
    /// name (`"WebFetch"`, …) so spend is attributable to the exact tool.
    Tool(String),
    /// Context compression / summarization of a session transcript.
    Compression,
    /// The out-of-band progress observer that narrates long turns.
    ProgressObserver,
    /// Conversation-title generation.
    Title,
    /// Memory subsystem sub-calls (recall, extraction).
    Memory,
    /// Platform-internal overhead no end user triggered (skill risk
    /// assessor, maintenance probes). Pairs with a `system:<component>`
    /// session id that names the specific subsystem.
    System,
}

impl CallReason {
    /// Stable wire/storage token. Unit variants map to a fixed lowercase
    /// word; `Tool(name)` becomes `tool:<name>` so the detail rides inside
    /// the one `reason` column. Round-trips through [`CallReason::parse`].
    pub fn to_token(&self) -> Cow<'static, str> {
        match self {
            CallReason::Chat => Cow::Borrowed("chat"),
            CallReason::Tool(name) => Cow::Owned(format!("{TOOL_TOKEN_PREFIX}{name}")),
            CallReason::Compression => Cow::Borrowed("compression"),
            CallReason::ProgressObserver => Cow::Borrowed("progress_observer"),
            CallReason::Title => Cow::Borrowed("title"),
            CallReason::Memory => Cow::Borrowed("memory"),
            CallReason::System => Cow::Borrowed("system"),
        }
    }

    /// Parse a stored token back into a reason. Unknown / legacy values
    /// (rows written before the column existed read as `None` from the
    /// store) are the caller's problem — see the read path in the libsql
    /// cost store, which maps them to [`CallReason::default`]. A bare
    /// `tool:` with no name parses to `Tool("")` rather than failing.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(name) = s.strip_prefix(TOOL_TOKEN_PREFIX) {
            return Some(CallReason::Tool(name.to_string()));
        }
        Some(match s {
            "chat" => CallReason::Chat,
            "compression" => CallReason::Compression,
            "progress_observer" => CallReason::ProgressObserver,
            "title" => CallReason::Title,
            "memory" => CallReason::Memory,
            "system" => CallReason::System,
            _ => return None,
        })
    }
}

// Flat-string serde so a `Tool(name)` never degrades to an object on the
// wire: it stays the same `tool:<name>` token used for storage. Lenient
// on read (unknown token -> default) to match the libsql read path.
impl Serialize for CallReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_token())
    }
}

impl<'de> Deserialize<'de> for CallReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(CallReason::parse(&s).unwrap_or_default())
    }
}

/// The smallest auditable billing unit.
///
/// Every record is associated with both a `job_id` and a `span_id` (the
/// LLM span that drove the spend), so the system never knows a cost
/// without knowing which call caused it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub user_id: String,
    pub session_id: SessionId,
    pub job_id: JobId,
    pub span_id: SpanId,
    /// Why this call was made. See [`CallReason`].
    pub reason: CallReason,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    /// Anthropic prompt-cache: input tokens served from the cache.
    /// Counted separately from `input_tokens` so cache-discounted pricing
    /// can be applied later. 0 for providers that don't report cache usage.
    pub cached_input_tokens: usize,
    /// Anthropic prompt-cache: input tokens written into the cache.
    pub cache_creation_input_tokens: usize,
    /// Spend for this single LLM call. Stored as integer micro-USD — see
    /// [`MicroUsd`] for the rationale (no float drift across aggregations
    /// or quota checks).
    pub cost_usd: MicroUsd,
    pub timestamp: DateTime<Utc>,
}

/// A half-open time range `[from, to)` used for cost queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// Aggregated cost information over a time range.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    pub total_cost_usd: MicroUsd,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub total_cached_input_tokens: usize,
    pub total_cache_creation_input_tokens: usize,
    pub record_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_reason_token_round_trips() {
        for reason in [
            CallReason::Chat,
            CallReason::Tool("WebFetch".to_string()),
            CallReason::Compression,
            CallReason::ProgressObserver,
            CallReason::Title,
            CallReason::Memory,
            CallReason::System,
        ] {
            let token = reason.to_token();
            assert_eq!(CallReason::parse(&token), Some(reason.clone()), "{token}");
        }
    }

    #[test]
    fn tool_token_carries_name() {
        let r = CallReason::Tool("Bash".to_string());
        assert_eq!(r.to_token(), "tool:Bash");
    }

    #[test]
    fn unknown_token_parses_to_none() {
        assert_eq!(CallReason::parse("definitely-not-a-reason"), None);
    }

    #[test]
    fn call_reason_serde_is_a_flat_string() {
        let r = CallReason::Tool("WebFetch".to_string());
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"tool:WebFetch\"");
        assert_eq!(serde_json::from_str::<CallReason>(&json).unwrap(), r);
    }
}
