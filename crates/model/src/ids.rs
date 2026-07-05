//! Strongly-typed identifiers used across the trace / job / session layers.
//!
//! `SessionId` is a caller-supplied opaque string (see `session.md` for the
//! naming conventions). `JobId`, `StepId`, `SpanId`, and `CostRecordId` are
//! ULIDs — 26-char Crockford base32, time-sortable, and rendered as their
//! canonical string form whenever they cross a serialization boundary.
//!
//! Every newtype is distinct at the type level so the compiler rejects
//! `fn load_job(id: SpanId)` calls. Use the explicit constructors and
//! conversions; raw `Ulid` does not coerce.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Opaque, caller-supplied session identifier.
///
/// Producers prefix to namespace by channel — e.g. `cli-<uuid>` or
/// `cron-<user>-<channel>`. The session manager treats it as a string key
/// and never inspects internal structure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn with_prefix(prefix: &str) -> Self {
        Self(format!("{prefix}{}", uuid::Uuid::new_v4()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl From<&SessionId> for String {
    fn from(value: &SessionId) -> Self {
        value.0.clone()
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for SessionId {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

/// Macro to declare a ULID-backed newtype with the standard surface
/// (constructors, accessors, `Display`, `FromStr`, serde transparent string).
macro_rules! ulid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        // Ord/PartialOrd come from the inner Ulid: ULIDs sort by
        // generation time first, then random tail — so a tuple key like
        // `(created_at, JobId)` gives a deterministic tie-break when
        // two ids share a microsecond `created_at`.
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Ulid);

        impl $name {
            /// Generate a fresh identifier.
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            /// Construct from an existing `Ulid`.
            pub fn from_ulid(id: Ulid) -> Self {
                Self(id)
            }

            /// Underlying `Ulid` value.
            pub fn as_ulid(&self) -> &Ulid {
                &self.0
            }

            /// Take the inner `Ulid`.
            pub fn into_inner(self) -> Ulid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = ulid::DecodeError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(s).map(Self)
            }
        }
    };
}

ulid_newtype! {
    /// Identifier for a `Job` — one externally-triggered unit of work.
    JobId
}

ulid_newtype! {
    /// Identifier for a `Step` — one iteration of the agent loop.
    StepId
}

ulid_newtype! {
    /// Identifier for a `Span` — one atomic action (LLM call or tool call).
    SpanId
}

ulid_newtype! {
    /// Identifier for a single `cost_records` audit row.
    CostRecordId
}

ulid_newtype! {
    /// Identifier for one task in a session's planning checklist.
    TaskId
}

/// Identifier for a parallel-tool batch within a `Step`.
///
/// Spans sharing the same `ParallelGroup` were dispatched concurrently;
/// their time windows may overlap. `None` on a span means the span ran
/// sequentially with the rest of its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParallelGroup(Ulid);

impl ParallelGroup {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn from_ulid(id: Ulid) -> Self {
        Self(id)
    }
}

impl Default for ParallelGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ParallelGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_as_string() {
        let id = SessionId::from("cli-abc123");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"cli-abc123\"");
        let back: SessionId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ulid_newtypes_are_distinct_types() {
        // Compile-time check — construction returns the right type.
        let _: JobId = JobId::new();
        let _: StepId = StepId::new();
        let _: SpanId = SpanId::new();
    }

    #[test]
    fn ulid_newtype_round_trips_via_string() {
        let id = JobId::new();
        let s = id.to_string();
        let parsed: JobId = s.parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn ulid_newtype_round_trips_via_serde() {
        let id = SpanId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: SpanId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn freshly_generated_ulid_newtypes_are_unique() {
        let a = JobId::new();
        let b = JobId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn parallel_group_round_trips() {
        let g = ParallelGroup::new();
        let s = serde_json::to_string(&g).unwrap();
        let back: ParallelGroup = serde_json::from_str(&s).unwrap();
        assert_eq!(back, g);
    }
}
