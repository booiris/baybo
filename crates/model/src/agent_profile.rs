//! User-managed agent profiles ("multi agents"): DB-backed chat personas
//! bundling a display identity, system prompt, an execution framework, and
//! an optional LLM pin.
//!
//! This module carries only the value types shared across layers; the row
//! shape and store port live in `baybo-store`, the sqlite impl in
//! `baybo-storage`. Distinct from the subagent profile registry
//! (`baybo-subagent`), which is filesystem-authoritative and types spawned
//! subagents. See `docs/modules/agent-profiles.md`.

use std::fmt;
use std::path::PathBuf;

use baybo_workspace::WorkspacePaths;
use serde::{Deserialize, Deserializer, Serialize};
use ulid::Ulid;

use crate::{BAYBO_BACKEND_TAG, ExternalAgentKind, SubagentBackendKind};

/// Fixed id of the seeded built-in profile representing default behavior
/// (workspace Soul prompt, default model, full skill and tool set). The row
/// is read-only except its avatar and cannot be deleted; the seed's
/// `INSERT OR IGNORE` in the sqlite store is the only writer of
/// `builtin = 1`.
pub const BUILTIN_AGENT_PROFILE_ID: &str = "baybo";

/// Upper bound on an agent profile's display name (chars, after trim),
/// enforced at the gateway before a create/update reaches the store.
/// Single source of truth for every validation site.
pub const MAX_AGENT_PROFILE_NAME_CHARS: usize = 64;

/// Upper bound on an agent profile *id*, which — unlike the display name —
/// becomes a directory name under the workspace `personas/` tree.
pub const MAX_AGENT_PROFILE_ID_CHARS: usize = 64;

/// An id that failed the [`AgentProfileId`] grammar. Carries the rejected
/// value so operator-facing errors can name it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid agent profile id {value:?}: {reason}")]
pub struct InvalidAgentProfileId {
    pub value: String,
    pub reason: &'static str,
}

/// Server-generated identifier for an agent profile.
///
/// A ULID at genesis (the fixed sentinel for the built-in row), and the
/// directory name of the profile's persona folder — so it is **not** an
/// opaque string: every construction path runs the same grammar,
/// `[A-Za-z0-9][A-Za-z0-9._-]{0,63}` (the skill-name grammar), which is
/// what keeps [`Self::soul_file`] and [`Self::skills_overlay_dir`] inside
/// the workspace. There is deliberately no infallible `From<String>`, and
/// `Deserialize` is not transparent: a guard only on the constructor would
/// be bypassed by every request body and stored row that parses an id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AgentProfileId(String);

impl AgentProfileId {
    /// Mint a fresh profile id (a ULID rendered as its canonical string).
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    /// The fixed id of the seeded built-in profile.
    pub fn builtin() -> Self {
        Self(BUILTIN_AGENT_PROFILE_ID.to_owned())
    }

    /// Validate `value` against the id grammar. The only fallible entry
    /// point; `TryFrom` and `Deserialize` both delegate here.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidAgentProfileId> {
        let value = value.into();
        let reject = |reason| InvalidAgentProfileId {
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
        if !chars
            .clone()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(reject(
                "may contain only ASCII letters, digits, '.', '_' and '-'",
            ));
        }
        if value.chars().count() > MAX_AGENT_PROFILE_ID_CHARS {
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

    /// Whether this is the seeded built-in profile — the one whose persona
    /// *is* the workspace's own declarative content.
    pub fn is_builtin(&self) -> bool {
        self.0 == BUILTIN_AGENT_PROFILE_ID
    }

    /// The `SOUL.md` this agent's system prompt reads. The built-in resolves
    /// to the workspace identity file, so an unbound session and a session
    /// bound to the built-in take byte-identical paths.
    pub fn soul_file(&self, paths: &WorkspacePaths) -> PathBuf {
        if self.is_builtin() {
            paths.identity_file(baybo_workspace::IdentityKind::Soul)
        } else {
            paths.persona_soul_file(&self.0)
        }
    }

    /// This agent's private skill overlay, or `None` for the built-in — whose
    /// skills are exactly the shared set, so an overlay pointing at
    /// `<workspace>/skills/` would register the same directory twice.
    pub fn skills_overlay_dir(&self, paths: &WorkspacePaths) -> Option<PathBuf> {
        (!self.is_builtin()).then(|| paths.persona_skills_dir(&self.0))
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AgentProfileId {
    type Error = InvalidAgentProfileId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for AgentProfileId {
    type Error = InvalidAgentProfileId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<AgentProfileId> for String {
    fn from(value: AgentProfileId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for AgentProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

/// What a new session is bound to at creation: which agent, and which
/// framework that agent ran on *at that moment*.
///
/// The two travel together because they are seeded by the same INSERT and
/// neither is ever written again — the id decides soul, skills and memory
/// partition; the framework is a snapshot, because a transcript written by
/// one framework cannot later be served by another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBinding {
    pub agent_id: AgentProfileId,
    pub framework: AgentFramework,
}

/// Execution framework an agent profile runs on: baybo's own agent loop or
/// one of the external agent CLIs.
///
/// The string forms are exactly the spawn protocol's backend tags
/// ([`BAYBO_BACKEND_TAG`] / [`ExternalAgentKind::as_str`]), so a stored
/// framework maps losslessly onto [`SubagentBackendKind`] when runtime
/// wiring arrives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFramework {
    #[default]
    Baybo,
    Claude,
    Codex,
}

impl AgentFramework {
    pub const ALL: &'static [AgentFramework] = &[Self::Baybo, Self::Claude, Self::Codex];

    /// Stable string form. Mirrors the `#[serde(rename_all = "snake_case")]`
    /// wire shape and the spawn protocol's backend tags.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baybo => BAYBO_BACKEND_TAG,
            Self::Claude => ExternalAgentKind::Claude.as_str(),
            Self::Codex => ExternalAgentKind::Codex.as_str(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == s)
    }

    /// Discriminator view for future runtime wiring (session binding,
    /// external-framework top-level sessions).
    pub fn to_backend_kind(self) -> SubagentBackendKind {
        match self {
            Self::Baybo => SubagentBackendKind::Baybo,
            Self::Claude => SubagentBackendKind::External(ExternalAgentKind::Claude),
            Self::Codex => SubagentBackendKind::External(ExternalAgentKind::Codex),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_id_round_trips_as_string() {
        let id = AgentProfileId::parse("agent-abc").unwrap();
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"agent-abc\"");
        let back: AgentProfileId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn generated_and_builtin_ids_pass_their_own_grammar() {
        for id in [AgentProfileId::generate(), AgentProfileId::builtin()] {
            AgentProfileId::parse(id.as_str())
                .unwrap_or_else(|e| panic!("minted id must be parseable: {e}"));
        }
    }

    #[test]
    fn profile_id_grammar_rejects_traversal_and_junk() {
        for bad in [
            "",
            "..",
            "../etc",
            "a/b",
            "a\\b",
            ".hidden",
            "-lead",
            "has space",
            "naïve",
            &"a".repeat(MAX_AGENT_PROFILE_ID_CHARS + 1),
        ] {
            assert!(
                AgentProfileId::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        for good in ["baybo", "01JABCDEF", "a", "a.b_c-d", &"a".repeat(64)] {
            assert!(
                AgentProfileId::parse(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }

    #[test]
    fn deserialize_enforces_the_grammar() {
        let err = serde_json::from_str::<AgentProfileId>("\"../escape\"")
            .expect_err("traversal must not deserialize");
        assert!(err.to_string().contains("invalid agent profile id"));
    }

    #[test]
    fn builtin_soul_is_the_workspace_identity_file_and_has_no_overlay() {
        let paths = baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/ws"));
        let builtin = AgentProfileId::builtin();
        assert_eq!(
            builtin.soul_file(&paths),
            paths.identity_file(baybo_workspace::IdentityKind::Soul)
        );
        assert!(builtin.skills_overlay_dir(&paths).is_none());

        let custom = AgentProfileId::parse("01JCUSTOM").unwrap();
        assert_eq!(
            custom.soul_file(&paths),
            paths.persona_soul_file("01JCUSTOM")
        );
        assert_eq!(
            custom.skills_overlay_dir(&paths),
            Some(paths.persona_skills_dir("01JCUSTOM"))
        );
    }

    #[test]
    fn generated_profile_ids_are_unique() {
        assert_ne!(AgentProfileId::generate(), AgentProfileId::generate());
    }

    #[test]
    fn builtin_id_matches_const() {
        assert_eq!(AgentProfileId::builtin().as_str(), BUILTIN_AGENT_PROFILE_ID);
    }

    #[test]
    fn framework_serde_uses_backend_tag_strings() {
        for f in AgentFramework::ALL.iter().copied() {
            let s = serde_json::to_string(&f).unwrap();
            assert_eq!(s, format!("\"{}\"", f.as_str()));
            let back: AgentFramework = serde_json::from_str(&s).unwrap();
            assert_eq!(back, f);
        }
    }

    #[test]
    fn framework_as_str_parse_round_trips() {
        for f in AgentFramework::ALL.iter().copied() {
            assert_eq!(AgentFramework::parse(f.as_str()), Some(f));
        }
        assert_eq!(AgentFramework::parse("unknown"), None);
    }

    #[test]
    fn framework_default_is_baybo() {
        assert_eq!(AgentFramework::default(), AgentFramework::Baybo);
        assert_eq!(AgentFramework::Baybo.as_str(), BAYBO_BACKEND_TAG);
    }

    #[test]
    fn framework_maps_onto_backend_kind() {
        assert_eq!(
            AgentFramework::Baybo.to_backend_kind(),
            SubagentBackendKind::Baybo
        );
        assert_eq!(
            AgentFramework::Codex.to_backend_kind(),
            SubagentBackendKind::External(ExternalAgentKind::Codex)
        );
    }
}
