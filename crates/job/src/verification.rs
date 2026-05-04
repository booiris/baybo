//! Verification substate of `JobStatus::Completed`.
//!
//! A normal chat job ends at `Completed { Unverified }` and is fully
//! terminal — no ceremony, no waiting. Only jobs created with a
//! `verifier: VerifierRef` declared can advance through
//! `Submitted → Accepted | Rejected`. See `job.md` for the rationale
//! (review flow / bounty / scored task).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Substate carried by `JobStatus::Completed`.
///
/// Advance via `Job::advance_verification`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum VerificationOutcome {
    /// Default for chat / cron / system jobs without a declared verifier.
    /// Terminal — no further movement permitted.
    Unverified,
    /// Submitted to the verifier; awaiting decision. Transient state in
    /// the verification chain (terminal w.r.t. actor execution; not
    /// terminal w.r.t. final outcome).
    Submitted {
        submitted_at: DateTime<Utc>,
        channel: SubmissionChannel,
    },
    /// Verifier accepted the result. Terminal.
    Accepted {
        decided_at: DateTime<Utc>,
        by: VerifierRef,
        evidence: Value,
    },
    /// Verifier rejected. **Not** the same as `JobStatus::Failed` — the
    /// agent did its work, the work just was not accepted. Terminal.
    Rejected {
        decided_at: DateTime<Utc>,
        by: VerifierRef,
        reason: String,
    },
}

impl VerificationOutcome {
    /// Whether the verification chain has reached a final outcome.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            VerificationOutcome::Unverified
                | VerificationOutcome::Accepted { .. }
                | VerificationOutcome::Rejected { .. }
        )
    }

    /// Snake-case wire tag matching the `#[serde(tag = "outcome", ...)]`
    /// annotation. Strips the variant data — for the full payload, use
    /// `serde_json::to_value`.
    pub fn outcome_tag(&self) -> &'static str {
        match self {
            VerificationOutcome::Unverified => "unverified",
            VerificationOutcome::Submitted { .. } => "submitted",
            VerificationOutcome::Accepted { .. } => "accepted",
            VerificationOutcome::Rejected { .. } => "rejected",
        }
    }

    /// Whether transitioning from `self` to `target` is a permitted
    /// verification advance. `Unverified` cannot advance — only jobs
    /// constructed with a declared `verifier` start in a state that
    /// permits movement.
    pub fn can_advance_to(&self, target: &VerificationOutcome) -> bool {
        matches!(
            (self, target),
            // From Submitted, you can land in Accepted or Rejected
            (
                VerificationOutcome::Submitted { .. },
                VerificationOutcome::Accepted { .. } | VerificationOutcome::Rejected { .. },
            )
        )
    }
}

/// Where a `Submitted` event originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionChannel {
    /// User clicked submit in the UI / TUI.
    Ui,
    /// Caller invoked the submit endpoint over the API.
    Api,
    /// The agent reached a self-determined "submit" condition (e.g. a
    /// review tool reported success).
    AutoTool,
}

/// Who is asked to decide acceptance.
///
/// `User`: a human at a specific channel + user id.
/// `External`: an external system (named in `system_name`).
/// `System`: an internal automated verifier (e.g. test runner, judge LLM).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifierRef {
    User { user_id: String },
    External { system_name: String },
    System { component: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submitted() -> VerificationOutcome {
        VerificationOutcome::Submitted {
            submitted_at: Utc::now(),
            channel: SubmissionChannel::Ui,
        }
    }

    fn accepted() -> VerificationOutcome {
        VerificationOutcome::Accepted {
            decided_at: Utc::now(),
            by: VerifierRef::User {
                user_id: "u1".into(),
            },
            evidence: serde_json::json!({}),
        }
    }

    fn rejected() -> VerificationOutcome {
        VerificationOutcome::Rejected {
            decided_at: Utc::now(),
            by: VerifierRef::System {
                component: "judge".into(),
            },
            reason: "missed criterion 3".into(),
        }
    }

    #[test]
    fn unverified_cannot_advance() {
        assert!(!VerificationOutcome::Unverified.can_advance_to(&submitted()));
        assert!(!VerificationOutcome::Unverified.can_advance_to(&accepted()));
    }

    #[test]
    fn submitted_advances_to_accepted_or_rejected() {
        assert!(submitted().can_advance_to(&accepted()));
        assert!(submitted().can_advance_to(&rejected()));
    }

    #[test]
    fn accepted_and_rejected_are_final() {
        assert!(accepted().is_final());
        assert!(rejected().is_final());
        assert!(VerificationOutcome::Unverified.is_final());
        assert!(!submitted().is_final());
    }

    #[test]
    fn round_trips_through_serde() {
        for v in [
            VerificationOutcome::Unverified,
            submitted(),
            accepted(),
            rejected(),
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let back: VerificationOutcome = serde_json::from_str(&s).unwrap();
            assert_eq!(std::mem::discriminant(&back), std::mem::discriminant(&v));
        }
    }
}
