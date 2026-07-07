//! User-managed agent profiles ("multi agents"): DB-backed chat personas
//! bundling a display identity, system prompt, an execution framework, and
//! an optional LLM pin.
//!
//! This module carries only the value types shared across layers; the row
//! shape and store port live in `baybo-store`, the libsql impl in
//! `baybo-storage`. Distinct from the subagent profile registry
//! (`baybo-subagent`), which is filesystem-authoritative and types spawned
//! subagents. See `docs/modules/agent-profiles.md`.

use std::fmt;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::{BAYBO_BACKEND_TAG, ExternalAgentKind, SubagentBackendKind};

/// Fixed id of the seeded built-in profile representing default behavior
/// (workspace Soul prompt, default model, full skill and tool set). The row
/// is read-only except its avatar and cannot be deleted; the seed's
/// `INSERT OR IGNORE` in the libsql store is the only writer of
/// `builtin = 1`.
pub const BUILTIN_AGENT_PROFILE_ID: &str = "baybo";

/// Upper bound on an agent profile's display name (chars, after trim),
/// enforced at the gateway before a create/update reaches the store.
/// Single source of truth for every validation site.
pub const MAX_AGENT_PROFILE_NAME_CHARS: usize = 64;

/// Server-generated identifier for an agent profile.
///
/// Opaque string (a ULID at genesis, the fixed sentinel for the built-in
/// row); the store and gateway treat it as a key and never inspect internal
/// structure. Mirrors [`crate::FolderId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentProfileId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for AgentProfileId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<AgentProfileId> for String {
    fn from(value: AgentProfileId) -> Self {
        value.0
    }
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
        let id = AgentProfileId::from("agent-abc");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"agent-abc\"");
        let back: AgentProfileId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
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
