//! User-approval gate for tool execution.
//!
//! A tool call flows through the gate before execution when any
//! [`ResourceAccess`] it declares is not already covered by an
//! [`ApprovedResource`] cached on the session. The gate returns an
//! [`ApprovalDecision`]; `ApproveAlways` instructs the caller to persist the
//! call's resources into session state for the rest of the session.
//!
//! Pure value types ([`ResourceAccess`], [`ApprovedResource`],
//! [`HostPattern`]) live in `baybo-model` so session state can persist them
//! without a cycle back through this crate.
//!
//! Implementations must be `Send + Sync` and safe to call concurrently — the
//! same gate is shared across tool calls that may run in parallel within a
//! single agent turn.
//!
//! The channel-backed gate (persist, enqueue, wake, await) lives in
//! `baybo-human-ask`; this module holds only the ports.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ChannelType, SessionId};
use dashmap::DashMap;

pub use baybo_model::approval::{
    ApprovalDecision, ApprovalRequest, ApprovalResolution, ApprovedResource, HostPattern,
    ResourceAccess,
};

use crate::ask::{AskCancel, AskSite};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// What came back from a gate, and *how*. The decision alone is what the
/// executor acts on; the resolution is what the ledgers downstream (the
/// issue timeline, span events) need so a torn-down prompt is not written
/// down as a human saying no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalOutcome {
    pub decision: ApprovalDecision,
    pub resolution: ApprovalResolution,
}

impl ApprovalOutcome {
    /// A decision somebody actually made on the approval surface.
    pub fn answered(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            resolution: ApprovalResolution::Answered,
        }
    }

    /// A standing rule decided without prompting anyone.
    pub fn policy(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            resolution: ApprovalResolution::Policy,
        }
    }

    /// The prompt was torn down undecided. Fail-closed.
    pub fn abandoned() -> Self {
        Self {
            decision: ApprovalDecision::Deny,
            resolution: ApprovalResolution::Abandoned,
        }
    }
}

const REFUSAL_ANSWERED: &str = r#"a human saw this prompt and said no"#;
const REFUSAL_ABANDONED: &str = r#"this prompt was torn down before anyone decided it — the turn was cancelled, or the process is shutting down"#;
const REFUSAL_POLICY: &str = r#"a standing policy denied this without asking anyone, because this session has no approval surface — every prompt here will be denied the same way"#;

/// Explain the denial source without trailing punctuation.
pub fn refusal_reason(resolution: ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::Answered => REFUSAL_ANSWERED,
        // `TimedOut` is historical: gates no longer expire, but old span
        // events still carry it.
        ApprovalResolution::Abandoned | ApprovalResolution::TimedOut => REFUSAL_ABANDONED,
        ApprovalResolution::Policy => REFUSAL_POLICY,
    }
}

/// Implemented by channel-side UIs (or an auto-deny fallback) to resolve
/// approval requests. Must be safe to call concurrently — a single agent turn
/// may dispatch multiple tool calls in parallel, each going through this gate
/// independently.
///
/// `site` says which channel's queue and which phase of the tool call the
/// prompt belongs to; `cancel` is what a parked prompt races. A gate never
/// times out on its own.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request(
        &self,
        req: ApprovalRequest,
        site: AskSite,
        cancel: AskCancel,
    ) -> ApprovalOutcome;
}

// ---------------------------------------------------------------------------
// AutoDenyGate
// ---------------------------------------------------------------------------

/// Fallback gate for channels without an approval UX. Denies every request.
/// Fail-closed is intentional: a silent auto-approve would defeat the point.
pub struct AutoDenyGate;

#[async_trait]
impl ApprovalGate for AutoDenyGate {
    async fn request(
        &self,
        _req: ApprovalRequest,
        _site: AskSite,
        _cancel: AskCancel,
    ) -> ApprovalOutcome {
        ApprovalOutcome::policy(ApprovalDecision::Deny)
    }
}

// ---------------------------------------------------------------------------
// ApprovalGateMap — per-channel gate resolution
// ---------------------------------------------------------------------------

/// Sync-accessible map of approval gates, shared between
/// `ChannelRegistry` (populates at registration time) and `ToolExecutor`
/// (reads at execution time).
///
/// Two tiers, matching the sidecar vs session-scoped client split in the
/// registry:
///
/// * **Type-level** gates, keyed by `ChannelType`. Populated by sidecar
///   registrations (e.g. one Telegram sidecar serving every session).
/// * **Session-level** gates, keyed by `(ChannelType, SessionId)`.
///   Populated by session-scoped client registrations (e.g. per-TUI
///   gates so each TUI only prompts its own user).
///
/// [`get`](Self::get) tries the session-level entry first and falls
/// back to the type-level gate; when neither is present it returns
/// `AutoDenyGate` (fail-closed).
pub struct ApprovalGateMap {
    type_level: DashMap<ChannelType, Arc<dyn ApprovalGate>>,
    session_level: DashMap<(ChannelType, SessionId), Arc<dyn ApprovalGate>>,
}

impl ApprovalGateMap {
    pub fn new() -> Self {
        Self {
            type_level: DashMap::new(),
            session_level: DashMap::new(),
        }
    }

    /// Register a type-level gate for a channel. Used for sidecar
    /// registrations.
    pub fn insert(&self, channel: ChannelType, gate: Arc<dyn ApprovalGate>) {
        self.type_level.insert(channel, gate);
    }

    /// Register a session-scoped gate for one `(ChannelType,
    /// SessionId)` pair. Used for session-scoped client registrations
    /// (e.g. a specific TUI instance).
    pub fn insert_session(
        &self,
        channel: ChannelType,
        session_id: SessionId,
        gate: Arc<dyn ApprovalGate>,
    ) {
        self.session_level.insert((channel, session_id), gate);
    }

    /// Evict a type-level gate. Called on sidecar unregister so tool
    /// calls scheduled after the channel disconnects fall back to the
    /// fail-closed `AutoDenyGate` instead of parking against a dead
    /// transport.
    pub fn remove(&self, channel: &ChannelType) {
        self.type_level.remove(channel);
    }

    /// Evict a session-scoped gate. Called on session-scoped client
    /// unregister. Cheap no-op if the entry is already gone.
    pub fn remove_session(&self, channel: &ChannelType, session_id: &SessionId) {
        self.session_level
            .remove(&(channel.clone(), session_id.clone()));
    }

    /// Resolve the gate for one tool call. Tries the session-scoped
    /// entry first (a specific TUI instance answering for its own
    /// session) and falls back to the type-level gate (a sidecar that
    /// answers for every session). When neither is registered returns
    /// `AutoDenyGate` (fail-closed).
    pub fn get(&self, channel: &ChannelType, session_id: &SessionId) -> Arc<dyn ApprovalGate> {
        if let Some(entry) = self
            .session_level
            .get(&(channel.clone(), session_id.clone()))
        {
            return Arc::clone(entry.value());
        }
        self.type_level
            .get(channel)
            .map(|e| Arc::clone(e.value()))
            .unwrap_or_else(|| Arc::new(AutoDenyGate))
    }
}

impl Default for ApprovalGateMap {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Heuristic JSON preview used by the agent layer when constructing an
/// [`ApprovalRequest`]. Truncates overly long strings so the UI does not have
/// to re-implement this.
pub fn preview_params(params: &serde_json::Value, max_len: usize) -> String {
    let s = serde_json::to_string(params).unwrap_or_else(|_| params.to_string());
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::human_ask::AskPhase;

    #[tokio::test]
    async fn auto_deny_gate_always_denies() {
        let gate = AutoDenyGate;
        let out = gate
            .request(
                ApprovalRequest {
                    tool_call_id: String::new(),
                    call_id: "x".into(),
                    session_id: "s".into(),
                    user_id: "u".into(),
                    tool: "t".into(),
                    accesses: vec![],
                    params_preview: String::new(),
                    description: None,
                },
                AskSite {
                    channel: ChannelType::tui(),
                    phase: AskPhase::BeforeExecute,
                },
                AskCancel::never(),
            )
            .await;
        assert_eq!(out, ApprovalOutcome::policy(ApprovalDecision::Deny));
    }

    #[test]
    fn every_live_resolution_gets_its_own_refusal_wording() {
        let all = [
            ApprovalResolution::Answered,
            ApprovalResolution::Abandoned,
            ApprovalResolution::Policy,
        ];
        let worded: Vec<&str> = all.into_iter().map(refusal_reason).collect();
        for (i, a) in worded.iter().enumerate() {
            for b in worded.iter().skip(i + 1) {
                assert_ne!(a, b, "two resolutions share one sentence");
            }
            assert!(!a.ends_with('.'), "callers punctuate: {a}");
        }
        assert_eq!(
            refusal_reason(ApprovalResolution::TimedOut),
            refusal_reason(ApprovalResolution::Abandoned),
            "historical expiries read as torn down, never as a human refusal"
        );
    }

    #[test]
    fn preview_truncates_long_params() {
        let v = serde_json::json!({ "s": "x".repeat(500) });
        let p = preview_params(&v, 64);
        assert!(p.len() <= 67);
        assert!(p.ends_with('…'));
    }
}
