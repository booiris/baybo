use std::path::Path;

use aura_workspace::{IdentityKind, WorkspaceManager, WorkspacePaths, absolutise};
use tracing::debug;

/// Framing preamble prepended to every runtime system prompt. Sets the
/// agent role and points at the per-attribute Edit affordance.
const TOP_HINT: &str = r#"You are an intelligent AI assistant. The following are your core attributes. You should use Edit tool to update the corresponding attribute file according to the conversation content."#;

/// Tail appended after every identity section. Lives at the very end so
/// it's the freshest piece of framing right before the conversation
/// begins — the model reads tag-handling guidance immediately before it
/// encounters the first message that may carry one.
const TAIL_HINT: &str = r#"Tool results and user messages may include <system-reminder> or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."#;

/// The Soul system loads personality and identity from workspace files
/// and produces the system prompt for LLM conversations.
pub struct Soul {
    system_prompt: String,
}

impl Soul {
    /// Build a Soul from workspace identity files. Frames the prompt
    /// with [`TOP_HINT`] up front (agent role + Edit affordance) and
    /// [`TAIL_HINT`] at the end (tag-handling guidance), with the
    /// identity files setting the voice in between. Workspace-shaped
    /// state (working directory, platform) lives on the Bash tool
    /// description; the real-time session id is surfaced through the
    /// aura-cli skill body — neither needs to ride here anymore.
    pub async fn from_workspace(workspace: &WorkspaceManager) -> anyhow::Result<Self> {
        let identity = workspace.load_identity_files().await?;
        let paths = WorkspacePaths::new(workspace.root.clone());
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

        let system_prompt = parts.join("\n\n");

        debug!(
            prompt_len = system_prompt.len(),
            "soul system prompt loaded"
        );
        Ok(Self { system_prompt })
    }

    /// Create a Soul with a custom system prompt. Used by callers that
    /// already know what they want (tests, gateway overrides) — the
    /// caller controls the entire prompt verbatim.
    pub fn custom(prompt: String) -> Self {
        Self {
            system_prompt: prompt,
        }
    }

    /// The rendered system prompt. Soul is session-independent now, so
    /// the same string is reused across every conversation built on
    /// this Soul instance.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

/// Wrap an identity-file body in an XML tag carrying the absolute
/// on-disk path. Explicit boundaries keep arbitrary user-authored
/// markdown inside one file from bleeding into a sibling section, and
/// surfacing the path lets the agent re-read or update the source file
/// without re-deriving its location.
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
    use aura_workspace::IdentityKind;

    #[tokio::test]
    async fn from_workspace_seeds_defaults_and_wraps_every_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceManager::new(dir.path().to_path_buf());
        let soul = Soul::from_workspace(&workspace).await.expect("soul");

        let prompt = soul.system_prompt();
        assert!(
            prompt.starts_with("You are an intelligent AI assistant."),
            "top hint must come first: {prompt}"
        );
        let hint_pos = 0;
        let soul_pos = prompt.find("<soul ").expect("soul tag present");
        let identity_pos = prompt.find("<identity ").expect("identity tag present");
        let user_pos = prompt
            .find("<user_profile ")
            .expect("user_profile tag present");
        let tail_pos = prompt
            .find("Tool results and user messages may include <system-reminder>")
            .expect("tail hint present");
        assert!(
            hint_pos < soul_pos
                && soul_pos < identity_pos
                && identity_pos < user_pos
                && user_pos < tail_pos,
            "expected hint < soul < identity < user_profile < tail: {prompt}"
        );
        assert!(
            prompt.trim_end().ends_with(
                "They bear no direct relation to the specific tool results or user messages in which they appear."
            ),
            "tail hint must close the prompt: {prompt}"
        );
        assert!(
            !prompt.contains("# Environment"),
            "env block must no longer live in the system prompt: {prompt}"
        );

        // Auto-seeding must have created the three identity files on disk
        // so the next session (or a direct Read) sees the same content.
        for kind in IdentityKind::all() {
            assert!(dir.path().join("profile").join(kind.file_name()).exists());
        }
    }

    #[tokio::test]
    async fn from_workspace_wraps_identity_files_with_absolute_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceManager::new(dir.path().to_path_buf());
        workspace.ensure_layout().await.expect("layout");
        workspace
            .write_identity_file(IdentityKind::Soul, "## Soul\nI am thoughtful.\n")
            .await
            .expect("write soul");
        workspace
            .write_identity_file(IdentityKind::Identity, "## Identity\nName: Aura.\n")
            .await
            .expect("write identity");

        let soul = Soul::from_workspace(&workspace).await.expect("soul");
        let prompt = soul.system_prompt();

        let hint_pos = prompt
            .find("You are an intelligent AI assistant.")
            .expect("top hint present");
        let soul_pos = prompt.find("I am thoughtful.").expect("soul text present");
        let identity_pos = prompt.find("Name: Aura.").expect("identity text present");
        assert_eq!(hint_pos, 0, "top hint must be at offset 0: {prompt}");
        assert!(
            hint_pos < soul_pos && soul_pos < identity_pos,
            "expected hint < soul < identity, got positions hint={hint_pos} soul={soul_pos} identity={identity_pos}"
        );

        let paths = WorkspacePaths::new(dir.path().to_path_buf());
        let soul_path = absolutise(&paths.identity_file(IdentityKind::Soul));
        let identity_path = absolutise(&paths.identity_file(IdentityKind::Identity));
        assert!(
            prompt.contains(&format!("<soul path=\"{}\">", soul_path.display())),
            "soul section must be wrapped with absolute path: {prompt}"
        );
        assert!(
            prompt.contains("</soul>") && prompt.contains("</identity>"),
            "wrapped sections must close their tags: {prompt}"
        );
        assert!(
            prompt.contains(&format!("<identity path=\"{}\">", identity_path.display())),
            "identity section must be wrapped with absolute path: {prompt}"
        );
    }

    #[test]
    fn custom_returns_prompt_verbatim() {
        let soul = Soul::custom("just this".to_string());
        assert_eq!(soul.system_prompt(), "just this");
    }
}
