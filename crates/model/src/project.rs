//! Kanban project value types: the container id and the issue id.
//!
//! Only the types shared across layers live here; row shapes and store
//! ports live in `baybo-store`, the sqlite impls in `baybo-storage`, and
//! the domain logic in `baybo-project`. See `docs/todo/kanban.md`.

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
///
/// A ULID at genesis, and the directory name of the project's folder under
/// `projects/` — so it is **not** opaque: every construction path runs the
/// same grammar, `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, which is what keeps
/// every derived path inside the workspace. `Deserialize` is deliberately
/// not transparent: a guard only on the constructor would be bypassed by
/// every request body and stored row that parses one.
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
///
/// Opaque (a ULID at genesis) — unlike [`ProjectId`] it never reaches the
/// filesystem, so a key is all it needs to be. The human-facing address is
/// its per-project `number`, and the REST surface only ever addresses an
/// issue as `(project, number)`; this id exists so child tables (runs,
/// events) can carry a single-column reference.
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
        // The whole reason `Deserialize` is hand-written: a request body or
        // a stored row must not be a way around `parse`.
        let refused = serde_json::from_str::<ProjectId>("\"../etc\"");
        assert!(refused.is_err(), "deserialization must re-run the grammar");
        let accepted: ProjectId =
            serde_json::from_str("\"01J9ABC\"").expect("a well-formed id deserializes");
        assert_eq!(accepted.as_str(), "01J9ABC");
    }
}
