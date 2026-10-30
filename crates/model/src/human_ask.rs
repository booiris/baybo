//! Value types of the human-ask lifecycle: one prompt to a human ("an ask"),
//! its payload, and how it resolved.
//!
//! Pure data shared by `baybo-store` (the `human_asks` ledger types its
//! columns with these) and the crates that raise and resolve asks. See
//! `docs/modules/human-ask.md`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::SessionId;
use crate::approval::{ApprovalDecision, ApprovalRequest};

/// Name of the tool that raises question asks.
pub const ASK_USER_TOOL_NAME: &str = "AskUser";

/// Identity of one ask. A UUID v4 when minted here; an ask raised through the
/// approval path reuses the [`ApprovalRequest::call_id`] it was raised with.
/// One tool call can raise several asks, so this — not the `tool_call_id` —
/// is the unique key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AskId(String);

/// An empty string is not an ask id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ask id must not be empty")]
pub struct InvalidAskId;

impl AskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Default for AskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AskId {
    type Err = InvalidAskId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(InvalidAskId);
        }
        Ok(Self(s.to_owned()))
    }
}

/// Which payload an ask carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// "May this tool call touch these resources?"
    Approval,
    /// The `AskUser` tool's multiple-choice questions.
    Question,
}

impl AskKind {
    /// Canonical db/wire spelling, matching the `snake_case` serde form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
        }
    }

    /// Parse the stored column back. `None` for an unknown value so a row
    /// written by a future variant is skipped rather than failing a listing.
    pub fn from_storage_str(s: &str) -> Option<Self> {
        match s {
            "approval" => Some(Self::Approval),
            "question" => Some(Self::Question),
            _ => None,
        }
    }
}

/// Where in a tool call's life the ask was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskPhase {
    /// The executor's pre-execute gate — the tool has not started.
    BeforeExecute,
    /// From inside `Tool::execute`, through a handle on the tool context.
    DuringExecute,
}

impl AskPhase {
    /// Canonical db/wire spelling, matching the `snake_case` serde form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BeforeExecute => "before_execute",
            Self::DuringExecute => "during_execute",
        }
    }

    /// Parse the stored column back. `None` for an unknown value.
    pub fn from_storage_str(s: &str) -> Option<Self> {
        match s {
            "before_execute" => Some(Self::BeforeExecute),
            "during_execute" => Some(Self::DuringExecute),
            _ => None,
        }
    }
}

/// Whether an ask's answer can still be applied after the tool future that
/// raised it is gone. A question has no state besides the wait; a
/// before-execute approval can re-dispatch the persisted `tool_use`. A
/// during-execute approval cannot: the command's partial state died with it.
pub fn resumable(kind: AskKind, phase: AskPhase) -> bool {
    kind == AskKind::Question || phase == AskPhase::BeforeExecute
}

/// The payload of one ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskRequest {
    Approval(ApprovalRequest),
    Question(QuestionRequest),
}

impl AskRequest {
    pub fn kind(&self) -> AskKind {
        match self {
            Self::Approval(_) => AskKind::Approval,
            Self::Question(_) => AskKind::Question,
        }
    }
}

/// The `AskUser` tool's input: one to four questions, each a tab in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Short tab label; also the key its [`QuestionAnswer`] is matched by.
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// The human's answer to one [`Question`]. `selected` and `other` both empty
/// means the question was skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub header: String,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// How an ask resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskResolution {
    /// A human answered an approval.
    Approval(ApprovalDecision),
    /// A human answered the questions.
    Answered(Vec<QuestionAnswer>),
    /// A human declined to answer the questions.
    Declined,
    /// Nobody answered: `/stop`, the card was closed, the user moved on, or
    /// the ask could not survive a restart.
    Abandoned,
    /// An unattended turn hit the gate; a standing rule denied it.
    PolicyDenied,
}

/// "Open a turn that applies this resolved ask" — sent when a human answers
/// an ask whose tool future no longer exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    pub session_id: SessionId,
    pub ask_id: AskId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumable_is_question_or_before_execute() {
        assert!(resumable(AskKind::Question, AskPhase::DuringExecute));
        assert!(resumable(AskKind::Question, AskPhase::BeforeExecute));
        assert!(resumable(AskKind::Approval, AskPhase::BeforeExecute));
        assert!(!resumable(AskKind::Approval, AskPhase::DuringExecute));
    }

    #[test]
    fn storage_strings_round_trip_and_match_serde() {
        for kind in [AskKind::Approval, AskKind::Question] {
            assert_eq!(AskKind::from_storage_str(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        for phase in [AskPhase::BeforeExecute, AskPhase::DuringExecute] {
            assert_eq!(AskPhase::from_storage_str(phase.as_str()), Some(phase));
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.as_str())
            );
        }
        assert_eq!(AskKind::from_storage_str("bogus"), None);
        assert_eq!(AskPhase::from_storage_str("bogus"), None);
    }

    #[test]
    fn ask_id_parses_and_displays() {
        let id: AskId = "abc".parse().unwrap();
        assert_eq!(id.to_string(), "abc");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc\"");
        assert!("".parse::<AskId>().is_err());
        assert_ne!(AskId::new(), AskId::new());
    }

    #[test]
    fn resolution_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(AskResolution::Approval(ApprovalDecision::ApproveAlways)).unwrap(),
            serde_json::json!({ "approval": "approve_always" })
        );
        assert_eq!(
            serde_json::to_value(AskResolution::PolicyDenied).unwrap(),
            serde_json::json!("policy_denied")
        );
        let answered = AskResolution::Answered(vec![QuestionAnswer {
            header: "Lang".into(),
            selected: vec!["Rust".into()],
            other: None,
        }]);
        let back: AskResolution =
            serde_json::from_str(&serde_json::to_string(&answered).unwrap()).unwrap();
        assert_eq!(back, answered);
    }

    #[test]
    fn approval_request_reads_input_without_tool_call_id() {
        let req: ApprovalRequest = serde_json::from_value(serde_json::json!({
            "call_id": "c",
            "session_id": "s",
            "tool": "Bash",
            "accesses": [],
            "params_preview": "{}",
        }))
        .unwrap();
        assert_eq!(req.tool_call_id, "");
        let ask = AskRequest::Approval(req);
        assert_eq!(ask.kind(), AskKind::Approval);
        let back: AskRequest = serde_json::from_str(&serde_json::to_string(&ask).unwrap()).unwrap();
        assert_eq!(back, ask);
    }
}
