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

use baybo_workspace::{IdentityKind, WorkspacePaths};
use serde::{Deserialize, Deserializer, Serialize};
use ulid::Ulid;

use crate::{BAYBO_BACKEND_TAG, ExternalAgentKind, SubagentBackendKind};

/// Fixed id of the seeded built-in profile representing default behavior
/// (workspace Soul prompt, default model, full skill and tool set). The row
/// is read-only except its avatar and cannot be deleted; the seed's
/// `INSERT OR IGNORE` in the sqlite store is the only writer of
/// `builtin = 1`.
pub const BUILTIN_AGENT_PROFILE_ID: &str = baybo_workspace::paths::BUILTIN_PERSONA_DIR;

/// Upper bound on an agent profile's display name (chars, after trim),
/// enforced at the gateway before a create/update reaches the store.
/// Single source of truth for every validation site.
pub const MAX_AGENT_PROFILE_NAME_CHARS: usize = 64;

/// Upper bound on an agent profile *id*, which — unlike the display name —
/// becomes a directory name under the workspace `personas/` tree.
pub const MAX_AGENT_PROFILE_ID_CHARS: usize = 64;

/// Upper bound on an [`AgentHandle`]. Short on purpose: a handle is typed
/// into comments and read off card faces, so it competes for the same room
/// as the text around it.
pub const MAX_AGENT_HANDLE_CHARS: usize = 32;

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
/// what keeps [`Self::identity_file`] and [`Self::skills_dir`] inside
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

    /// Where this agent reads one identity file from.
    ///
    /// All three belong to the agent, including `USER.md`: what an agent has
    /// worked out about the human is its own accumulated notes, and sharing
    /// them would be a write channel between agents that the memory partition
    /// does not cover. The *shared* profile — the stable facts the operator
    /// curates — stays at `personas/USER.md` and every agent reads it too; see
    /// `baybo_context::prompts::soul`.
    ///
    /// One rule, no special cases: an agent's files live in its own
    /// directory, the built-in's at `personas/baybo/`. The shared human
    /// profile (`personas/USER.md`) is not addressed here — it belongs to no
    /// agent, so it is not one of anyone's identity files.
    pub fn identity_file(&self, paths: &WorkspacePaths, kind: IdentityKind) -> PathBuf {
        paths.persona_identity_file(&self.0, kind)
    }

    /// This agent's skills, at `personas/<id>/skills/`.
    ///
    /// Every agent owns its set outright — there is no shared tree to inherit
    /// from or be shadowed by, and the built-in is not a special case: its
    /// skills live at `personas/baybo/skills/` like anyone else's. The only
    /// skills an agent sees that are not in this directory are the ones
    /// compiled into the binary, which belong to the process rather than to
    /// any persona.
    pub fn skills_dir(&self, paths: &WorkspacePaths) -> PathBuf {
        paths.persona_skills_dir(&self.0)
    }

    /// This agent's memory tree — partitioned per agent by construction,
    /// with no shared tree for one agent's writes to land in, exactly like
    /// [`Self::skills_dir`].
    pub fn memory_dir(&self, paths: &WorkspacePaths) -> PathBuf {
        paths.persona_memory_dir(&self.0)
    }

    /// The index of this agent's memory — the one memory file the system
    /// prompt carries verbatim.
    pub fn memory_index_file(&self, paths: &WorkspacePaths) -> PathBuf {
        paths.persona_memory_index_file(&self.0)
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

/// A handle that failed the [`AgentHandle`] grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid agent handle {value:?}: {reason}")]
pub struct InvalidAgentHandle {
    pub value: String,
    pub reason: &'static str,
}

/// What a project agent is called on its board: `@lead`, `@dev-1`.
///
/// Deliberately not the profile id. The id is a ULID that names a directory
/// on disk; the handle is what a person types into a comment, so it is
/// short, lowercase, and readable. It is also **immutable and permanently
/// reserved within its project** — a timeline entry saying "@dev-1 moved
/// this" has to keep meaning the same agent after that agent is removed,
/// which it cannot if the handle is ever reissued.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AgentHandle(String);

impl AgentHandle {
    /// Validate `value` against the handle grammar:
    /// `[a-z][a-z0-9-]{0,31}`, no trailing dash.
    ///
    /// Narrower than the id grammar in every direction — no uppercase, no
    /// dots, no underscores — because a handle is read aloud and typed from
    /// memory. Two handles that differ only in case would be one name to
    /// every reader and two to the index.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidAgentHandle> {
        let value = value.into();
        let reject = |reason| InvalidAgentHandle {
            value: value.clone(),
            reason,
        };
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(reject("empty"));
        };
        if !first.is_ascii_lowercase() {
            return Err(reject("must start with a lowercase ASCII letter"));
        }
        if !chars
            .clone()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(reject(
                "may contain only lowercase ASCII letters, digits and '-'",
            ));
        }
        if value.ends_with('-') {
            return Err(reject("must not end with '-'"));
        }
        if value.chars().count() > MAX_AGENT_HANDLE_CHARS {
            return Err(reject("longer than 32 characters"));
        }
        Ok(Self(value))
    }

    /// Reduce a display name to handle shape, or `None` if nothing usable
    /// survives. Not infallible on purpose: a name of pure punctuation has
    /// no handle, and inventing one (`agent-01J…`) would produce exactly the
    /// unreadable identifier the handle exists to avoid — the caller should
    /// ask for a different name instead.
    pub fn derive(name: &str) -> Option<Self> {
        let mut slug = String::with_capacity(name.len());
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() {
                slug.extend(ch.to_lowercase());
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-');
        // A leading digit survives slugification but not the grammar, so
        // truncate first and let `parse` be the single judge.
        let slug: String = slug.chars().take(MAX_AGENT_HANDLE_CHARS).collect();
        Self::parse(slug.trim_end_matches('-').to_owned()).ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

/// An agent's place on a project team: which board, and the handle it
/// answers to there.
///
/// One field rather than two nullable columns' worth of `Option`, because
/// the two facts are never independent — an agent with a board but no
/// handle cannot be mentioned, and a handle with no board is scoped to
/// nothing. `None` on the profile means a global agent, which has neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMembership {
    pub project_id: crate::ProjectId,
    pub handle: AgentHandle,
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
    fn the_builtin_is_just_another_persona_directory() {
        let paths = baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/ws"));
        let builtin = AgentProfileId::builtin();
        for kind in IdentityKind::all() {
            assert_eq!(
                builtin.identity_file(&paths, kind),
                paths.persona_identity_file(BUILTIN_AGENT_PROFILE_ID, kind),
            );
        }
        // The shared human profile is nobody's identity file.
        assert_ne!(
            builtin.identity_file(&paths, IdentityKind::User),
            paths.shared_user_file()
        );
        // The built-in is not a special case here either: its skills live in
        // its own persona directory, like every other agent's.
        assert_eq!(
            builtin.skills_dir(&paths),
            paths.persona_skills_dir(baybo_workspace::paths::BUILTIN_PERSONA_DIR)
        );
    }

    #[test]
    fn a_custom_agent_owns_all_three_identity_files() {
        let paths = baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from("/ws"));
        let custom = AgentProfileId::parse("01JCUSTOM").unwrap();

        // Including USER.md: those are this agent's own notes about the
        // human. The shared profile every agent also reads is a separate
        // section, assembled from `personas/USER.md`.
        for kind in IdentityKind::all() {
            assert_eq!(
                custom.identity_file(&paths, kind),
                paths.persona_identity_file("01JCUSTOM", kind),
                "{kind:?} belongs to the agent"
            );
        }
        assert_eq!(
            custom.skills_dir(&paths),
            paths.persona_skills_dir("01JCUSTOM")
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
    fn handle_grammar_is_narrower_than_the_id_grammar() {
        for good in ["lead", "dev-1", "a", &"a".repeat(MAX_AGENT_HANDLE_CHARS)] {
            assert!(
                AgentHandle::parse(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
        for bad in [
            "",
            "Lead",  // an id may be mixed-case; a handle may not
            "dev_1", // nor may it use the id grammar's '_' and '.'
            "dev.1",
            "1dev",
            "-lead",
            "lead-",
            "has space",
            "naïve",
            &"a".repeat(MAX_AGENT_HANDLE_CHARS + 1),
        ] {
            assert!(
                AgentHandle::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn handles_derive_from_display_names() {
        let derive = |name: &str| AgentHandle::derive(name).map(|h| h.as_str().to_owned());
        assert_eq!(derive("Lead"), Some("lead".to_owned()));
        assert_eq!(
            derive("  Test Engineer  "),
            Some("test-engineer".to_owned())
        );
        assert_eq!(derive("dev_1"), Some("dev-1".to_owned()));
        // Truncation must not leave the trailing dash the grammar refuses.
        let long = derive(&format!("{} tail", "a".repeat(MAX_AGENT_HANDLE_CHARS - 1)));
        assert_eq!(long, Some("a".repeat(MAX_AGENT_HANDLE_CHARS - 1)));
        // Nothing usable is `None`, not an invented identifier: the point of
        // a handle is that a person can read it.
        assert_eq!(derive("!!!"), None);
        assert_eq!(derive("42"), None, "a handle cannot start with a digit");
    }

    #[test]
    fn every_derived_handle_passes_the_grammar() {
        for name in ["Lead", "Dev 1", "QA/Test", "a-----b", "Ünïcödé name"] {
            if let Some(handle) = AgentHandle::derive(name) {
                AgentHandle::parse(handle.as_str())
                    .unwrap_or_else(|e| panic!("derive({name:?}) produced an invalid handle: {e}"));
            }
        }
    }

    #[test]
    fn handle_round_trips_as_a_transparent_string() {
        let handle = AgentHandle::parse("dev-1").unwrap();
        let s = serde_json::to_string(&handle).unwrap();
        assert_eq!(s, "\"dev-1\"");
        assert_eq!(serde_json::from_str::<AgentHandle>(&s).unwrap(), handle);
        assert!(
            serde_json::from_str::<AgentHandle>("\"Dev 1\"").is_err(),
            "deserialize must enforce the grammar too"
        );
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
