//! System-prompt (Soul) assembly: the framing that wraps the workspace
//! identity files into the leading `Role::System` row.
//!
//! Moved here from the agent layer so the system-prompt framing lives with
//! the rest of the prompt-injection text. `ContextManager` owns the
//! lifecycle (seed + reseed-after-compaction); this module is the pure
//! assembly seam.

use std::path::Path;

use baybo_workspace::{IdentityKind, WorkspacePaths, absolutise};

/// Minimal fallback used when the workspace soul can't be assembled (an I/O
/// error — identity files normally auto-seed, so this is a last resort). Lives
/// here so the fallback travels with the assembly it backs.
pub const FALLBACK_SYSTEM_PROMPT: &str = "You are Baybo, an intelligent assistant.";

/// Framing preamble prepended to every runtime system prompt. Sets the
/// agent role and points at the per-attribute Edit affordance.
const TOP_HINT: &str = r#"You are an intelligent AI assistant. The following are your core attributes. You should use Edit tool to update the corresponding attribute file according to the conversation content."#;

/// Tail appended after every identity section. Lives at the very end so it's
/// the freshest piece of framing right before the conversation begins — the
/// model reads tag-handling guidance immediately before it encounters the
/// first message that may carry one.
const TAIL_HINT: &str = r#"Tool results and user messages may include <system-reminder> or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."#;

/// Assemble the system prompt from the workspace identity files: [`TOP_HINT`]
/// up front (agent role + Edit affordance), the three identity sections
/// (`soul` / `identity` / `user_profile`), then [`TAIL_HINT`]
/// (tag-handling guidance). Reads the files via the auto-seeding free
/// `load_identity_files`, so a deleted file is recreated rather than left
/// half-formed.
pub async fn assemble_from_workspace(paths: &WorkspacePaths) -> anyhow::Result<String> {
    let identity = baybo_workspace::identity::load_identity_files(paths.root()).await?;
    let parts = [
        TOP_HINT.to_string(),
        wrap_section(
            "soul",
            &paths.identity_file(IdentityKind::Soul),
            &identity.soul,
        ),
        wrap_section(
            "identity",
            &paths.identity_file(IdentityKind::Identity),
            &identity.identity,
        ),
        wrap_section(
            "user_profile",
            &paths.identity_file(IdentityKind::User),
            &identity.user,
        ),
        TAIL_HINT.to_string(),
    ];
    Ok(parts.join("\n\n"))
}

/// Wrap an identity-file body in an XML tag carrying the absolute on-disk
/// path. Explicit boundaries keep arbitrary user-authored markdown inside one
/// file from bleeding into a sibling section, and surfacing the path lets the
/// agent re-read or update the source file without re-deriving its location.
fn wrap_section(tag: &str, path: &Path, body: &str) -> String {
    let abs = absolutise(path);
    format!(
        "<{tag} path=\"{path}\">\n{body}\n</{tag}>",
        path = abs.display(),
        body = body.trim_end_matches('\n'),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn assembles_hint_sections_and_tail_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(dir.path().to_path_buf());
        let prompt = assemble_from_workspace(&paths).await.expect("assemble");

        assert!(prompt.starts_with("You are an intelligent AI assistant."));
        let soul = prompt.find("<soul ").expect("soul tag");
        let identity = prompt.find("<identity ").expect("identity tag");
        let user = prompt.find("<user_profile ").expect("user_profile tag");
        let tail = prompt
            .find("Tool results and user messages may include <system-reminder>")
            .expect("tail hint");
        assert!(soul < identity && identity < user && user < tail);
        assert!(prompt.trim_end().ends_with(
            "They bear no direct relation to the specific tool results or user messages in which they appear."
        ));
    }
}
