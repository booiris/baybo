//! Strongly-typed identifiers used across the trace / turn / session layers.
//!
//! `SessionId` is a caller-supplied opaque string (see `session.md` for the
//! naming conventions). `TurnId`, `StepId`, `SpanId`, and `CostRecordId` are
//! ULIDs — 26-char Crockford base32, time-sortable, and rendered as their
//! canonical string form whenever they cross a serialization boundary.
//!
//! Every newtype is distinct at the type level so the compiler rejects
//! `fn load_turn(id: SpanId)` calls. Use the explicit constructors and
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

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
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
        // `(created_at, TurnId)` gives a deterministic tie-break when
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
    /// Identifier for a `Turn` — one externally-triggered unit of work.
    TurnId
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

/// Content address of one serialized tool-definition set.
///
/// An `LlmCall` span references the tool set it offered the model by this
/// hash instead of embedding it: the set is session-stable and tens of KB,
/// so an inline copy per call would dwarf the span that carries it. The
/// digest itself is computed by `baybo-trace`, which owns the definitions
/// and their canonical serialization.
///
/// Only a 64-char lowercase-hex digest is representable — [`FromStr`]
/// rejects anything else, so a value arriving from a URL path or a stored
/// row cannot smuggle in a free-form string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolSetHash(String);

/// Length of a hex-rendered SHA-256 digest.
const TOOL_SET_HASH_LEN: usize = 64;

impl ToolSetHash {
    /// Render a raw SHA-256 digest as the canonical lowercase hex form.
    pub fn from_digest(digest: &[u8; 32]) -> Self {
        use fmt::Write as _;
        let mut out = String::with_capacity(TOOL_SET_HASH_LEN);
        for byte in digest {
            // Infallible: `String`'s `Write` never errors.
            let _ = write!(out, "{byte:02x}");
        }
        Self(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolSetHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a string is not a tool-set hash. Distinct from `ulid::DecodeError`
/// so the gateway can map it to a 400 with a message that names the shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("expected a {TOOL_SET_HASH_LEN}-char lowercase hex digest")]
pub struct ToolSetHashParseError;

impl FromStr for ToolSetHash {
    type Err = ToolSetHashParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != TOOL_SET_HASH_LEN
            || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(ToolSetHashParseError);
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for ToolSetHash {
    type Error = ToolSetHashParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ToolSetHash> for String {
    fn from(value: ToolSetHash) -> Self {
        value.0
    }
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
        let _: TurnId = TurnId::new();
        let _: StepId = StepId::new();
        let _: SpanId = SpanId::new();
    }

    #[test]
    fn ulid_newtype_round_trips_via_string() {
        let id = TurnId::new();
        let s = id.to_string();
        let parsed: TurnId = s.parse().unwrap();
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
        let a = TurnId::new();
        let b = TurnId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn tool_set_hash_round_trips_as_a_bare_string() {
        let hash = ToolSetHash::from_digest(&[0xab; 32]);
        assert_eq!(hash.as_str().len(), TOOL_SET_HASH_LEN);
        let s = serde_json::to_string(&hash).unwrap();
        assert!(s.starts_with("\"abab"), "expected a hex string, got {s}");
        let back: ToolSetHash = serde_json::from_str(&s).unwrap();
        assert_eq!(back, hash);
    }

    #[test]
    fn tool_set_hash_rejects_anything_but_lowercase_hex() {
        for bad in [
            "",
            "not-a-hash",
            &"a".repeat(TOOL_SET_HASH_LEN - 1),
            &"a".repeat(TOOL_SET_HASH_LEN + 1),
            &"A".repeat(TOOL_SET_HASH_LEN),
            &"g".repeat(TOOL_SET_HASH_LEN),
        ] {
            assert_eq!(
                bad.parse::<ToolSetHash>(),
                Err(ToolSetHashParseError),
                "{bad:?} should not parse as a tool-set hash"
            );
        }
        // Deserialization goes through the same gate, so a hand-edited row
        // cannot reintroduce a free-form string.
        assert!(serde_json::from_str::<ToolSetHash>("\"nope\"").is_err());
    }

    #[test]
    fn parallel_group_round_trips() {
        let g = ParallelGroup::new();
        let s = serde_json::to_string(&g).unwrap();
        let back: ParallelGroup = serde_json::from_str(&s).unwrap();
        assert_eq!(back, g);
    }
}
