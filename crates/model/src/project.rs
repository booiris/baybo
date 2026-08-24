//! Kanban project value types: the container id and the issue id.

use std::fmt;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Upper bound on a project's display name (chars, after trim).
pub const MAX_PROJECT_NAME_CHARS: usize = 64;

/// Upper bound on a project *id*, which becomes a directory name under the
/// workspace `projects/` tree.
pub const MAX_PROJECT_ID_CHARS: usize = 64;

/// A value that failed one of this module's grammars. Carries the rejected
/// value so operator-facing errors can name it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} {value:?}: {reason}")]
pub struct InvalidProjectValue {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

/// Server-generated identifier for a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    /// Validate `value` against the id grammar. The only fallible entry
    /// point; `TryFrom` and `Deserialize` both delegate here.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidProjectValue> {
        let value = value.into();
        let reject = |reason| InvalidProjectValue {
            kind: "project id",
            value: value.clone(),
            reason,
        };
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(reject("empty"));
        };
        if !first.is_ascii_alphanumeric() {
            return Err(reject("must start with an ASCII letter or digit"));
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(reject(
                "may contain only ASCII letters, digits, '.', '_' and '-'",
            ));
        }
        if value.chars().count() > MAX_PROJECT_ID_CHARS {
            return Err(reject("longer than 64 characters"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProjectId {
    type Error = InvalidProjectValue;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for ProjectId {
    type Error = InvalidProjectValue;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ProjectId> for String {
    fn from(value: ProjectId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

/// Server-generated identifier for one issue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueId(String);

impl IssueId {
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for IssueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for IssueId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for IssueId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<IssueId> for String {
    fn from(value: IssueId) -> Self {
        value.0
    }
}

/// Server-generated identifier for one timeline entry. Opaque, and ULID so
/// entries written in the same microsecond still have a stable order to
/// break the tie on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueEventId(String);

impl IssueEventId {
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IssueEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for IssueEventId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Server-generated identifier for one execution of an issue. Opaque; the
/// UI addresses a run by its attempt number within its issue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueRunId(String);

impl IssueRunId {
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for IssueRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for IssueRunId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for IssueRunId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<IssueRunId> for String {
    fn from(value: IssueRunId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_pass_their_own_grammar() {
        let id = ProjectId::generate();
        ProjectId::parse(id.as_str())
            .expect("a minted id must satisfy the grammar it is checked by");
    }

    #[test]
    fn path_traversal_shapes_are_refused() {
        for bad in ["", "..", "../escape", "a/b", ".hidden/../..", "with space"] {
            assert!(
                ProjectId::parse(bad).is_err(),
                "{bad:?} must not survive the id grammar"
            );
        }
    }

    #[test]
    fn deserialize_runs_the_grammar() {
        let refused = serde_json::from_str::<ProjectId>("\"../etc\"");
        assert!(refused.is_err(), "deserialization must re-run the grammar");
        let accepted: ProjectId =
            serde_json::from_str("\"01J9ABC\"").expect("a well-formed id deserializes");
        assert_eq!(accepted.as_str(), "01J9ABC");
    }
}
