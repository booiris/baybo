//! [`SubagentProfile`] — typed subagent definition.
//!
//! Each profile fully describes what a child `AgentActor` should look
//! like when spawned by `spawn_subagent`:
//!
//! * `name` — the value the parent LLM emits as `subagent_type`.
//! * `description` — short blurb listing "when to use / when not to use".
//!   Concatenated into the `spawn_subagent` tool description so the
//!   parent LLM can pick between types.
//! * `system_prompt` — REPLACES the parent's [`Soul`] for the child
//!   actor. The author owns the full system contract (identity,
//!   security, output conventions); base Soul is not threaded through.
//!   See `docs/grilling/subagent-redesign.md` (TBD).
//! * `default_tier` — `ModelTier` resolved when the spawn call omits
//!   both `model_tier` and `llm`. The pool default fires only when this
//!   too is `None`.
//!
//! Profiles are loaded as a single `<name>.md` markdown file with a
//! YAML frontmatter block — see [`crate::loader`].
//!
//! [`Soul`]: https://example.invalid

use std::path::PathBuf;

use baybo_model::{ArtifactSource, ModelTier, TrustLevel};
use serde::{Deserialize, Serialize};

/// Declarative subagent type definition. Cloneable so the registry can
/// hand out owned copies to the router on every spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentProfile {
    pub name: String,
    pub version: String,
    /// Short blurb shown to the parent LLM in the dynamic
    /// `spawn_subagent` description. Should answer "when do I pick
    /// this type? when do I NOT?".
    pub description: String,
    /// Full system prompt for the child actor. Replaces Soul. Plain
    /// markdown body of the source file (post-frontmatter), with
    /// trailing whitespace trimmed.
    pub system_prompt: String,
    /// Default model tier for this profile. `None` ⇒ fall through to
    /// the pool default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<ModelTier>,
    /// Where this profile came from — built-in, workspace dir, or a
    /// future extension registry.
    pub source: ArtifactSource,
    /// Trust level. Built-ins and workspace profiles are Trusted;
    /// future installed profiles ride Installed.
    pub trust_level: TrustLevel,
    /// On-disk path the profile was loaded from, when known. Used by
    /// the upcoming risk-assessor pass and surfaced in audit traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    /// The only tools a child on this profile is offered, or `None` for
    /// every tool its channel and trigger already allow. A profile that
    /// names a set pays for it: an omitted tool is invisible AND refused,
    /// so the set has to cover what the body tells the child to do.
    ///
    /// `None` rather than an empty vec is the unrestricted case, so a
    /// profile written before this key — or one from an external source
    /// that never learned it — keeps what it has today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

/// Lightweight projection used by hot paths that only need
/// `name` + `description` + `default_tier` — namely the
/// `spawn_subagent` description renderer that runs every LLM turn.
/// Skips cloning the full `system_prompt` body.
#[derive(Debug, Clone)]
pub struct SubagentProfileSummary {
    pub name: String,
    pub description: String,
    pub default_tier: Option<ModelTier>,
}

impl From<&SubagentProfile> for SubagentProfileSummary {
    fn from(p: &SubagentProfile) -> Self {
        Self {
            name: p.name.clone(),
            description: p.description.clone(),
            default_tier: p.default_tier,
        }
    }
}
