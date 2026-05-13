use std::path::Path;

use aura_workspace::{IdentityKind, WorkspaceManager, WorkspacePaths, absolutise};
use tracing::debug;

/// Framing preamble prepended to every runtime system prompt. Sets the
/// agent role and operating context before the user-editable identity
/// files set the voice — soul/identity/user_profile own personality
/// and preferences; this const owns structural facts about the system
/// the agent is running inside.
const TOP_HINT: &str = r#"You are an intelligent AI assistant. The following are your core attributes. You should use Edit tool to update the corresponding attribute file according to the conversation content."#;

/// The Soul system loads personality and identity from workspace files
/// and produces the system prompt for LLM conversations.
pub struct Soul {
    system_prompt: String,
}

impl Soul {
    /// Build a Soul from workspace identity files. Always prepends a
    /// fixed top hint (agent role + structural facts) and appends an
    /// environment block describing the workspace layout — the LLM
    /// uses the env block to construct absolute paths inside the
    /// working directory so tool calls land where the OS sandbox is
    /// rooted. The env block goes last so the identity files set the
    /// voice up front and the runtime details are the freshest piece
    /// of context before the user message.
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
            build_env_block(workspace),
        ];

        let system_prompt = parts.join("\n\n");

        debug!(
            prompt_len = system_prompt.len(),
            "soul system prompt loaded"
        );
        Ok(Self { system_prompt })
    }

    /// Create a Soul with a custom system prompt. Used by callers that
    /// already know what they want (tests, gateway overrides) — no env
    /// block is injected; the caller controls the entire prompt.
    pub fn custom(prompt: String) -> Self {
        Self {
            system_prompt: prompt,
        }
    }

    /// Render the system prompt for `session_id`, substituting the
    /// session placeholder embedded in the env block. The Soul is
    /// built before any session exists, so the env block carries a
    /// `{{session_id}}` token that gets replaced here. Custom prompts
    /// without the placeholder pass through unchanged.
    pub fn system_prompt(&self, session_id: &str) -> String {
        self.system_prompt
            .replace(SESSION_ID_PLACEHOLDER, session_id)
    }

    /// Raw prompt template with the `{{session_id}}` placeholder
    /// intact. Used by boot paths that capture the prompt before any
    /// session exists (e.g. runtime startup) and then hand the
    /// captured string to a per-session [`Self::custom`] so each
    /// session's [`Self::system_prompt`] does the substitution.
    pub fn raw_template(&self) -> &str {
        &self.system_prompt
    }
}

const SESSION_ID_PLACEHOLDER: &str = "{{session_id}}";
const WORK_DIR_PLACEHOLDER: &str = "{{work_dir}}";
const PLATFORM_PLACEHOLDER: &str = "{{platform}}";

const ENV_BLOCK_TEMPLATE: &str = r#"# Environment
- Working directory: {{work_dir}}
- Platform: {{platform}}
- Session ID: {{session_id}}

Tool results and user messages may include <system-reminder> or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."#;

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

/// Render the agent-facing environment block. Mirrors what `runtime.rs`
/// hands the tool layer: `ToolContext.workspace_root` is the workspace
/// `work/` subdirectory (also the OS sandbox FS scope), and the
/// surrounding `<root>` carries identity / state / logs. The work
/// dir is absolutised before rendering — a relative workspace root
/// (e.g. the debug-build default `./.aura`) would otherwise leak a
/// cwd-relative path into the prompt and the agent has no way to
/// know what cwd the runtime started from. The session-id placeholder
/// is left intact here so [`Soul::system_prompt`] can substitute the
/// per-conversation id at call time.
fn build_env_block(workspace: &WorkspaceManager) -> String {
    let paths = WorkspacePaths::new(workspace.root.clone());
    let work_dir = absolutise(&paths.work_dir());
    ENV_BLOCK_TEMPLATE
        .replace(WORK_DIR_PLACEHOLDER, &work_dir.display().to_string())
        .replace(PLATFORM_PLACEHOLDER, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_workspace::IdentityKind;
    use std::path::PathBuf;

    const TEST_SESSION_ID: &str = "sess-test-123";

    #[tokio::test]
    async fn from_workspace_seeds_defaults_and_wraps_every_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceManager::new(dir.path().to_path_buf());
        let soul = Soul::from_workspace(&workspace).await.expect("soul");

        let prompt = soul.system_prompt(TEST_SESSION_ID);
        let expected_work_dir =
            absolutise(&WorkspacePaths::new(dir.path().to_path_buf()).work_dir())
                .display()
                .to_string();
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
        let env_pos = prompt.find("# Environment").expect("env block present");
        assert!(
            hint_pos < soul_pos
                && soul_pos < identity_pos
                && identity_pos < user_pos
                && user_pos < env_pos,
            "expected hint < soul < identity < user_profile < env: {prompt}"
        );
        assert!(
            prompt.contains(&expected_work_dir),
            "missing work_dir {expected_work_dir} in: {prompt}"
        );
        assert!(
            prompt.contains(std::env::consts::OS),
            "missing platform in: {prompt}"
        );

        // Auto-seeding must have created the three identity files on disk
        // so the next session (or a direct Read) sees the same content.
        for kind in IdentityKind::all() {
            assert!(dir.path().join("profile").join(kind.file_name()).exists());
        }
    }

    #[tokio::test]
    async fn from_workspace_emits_absolute_paths_for_relative_root() {
        // Debug-build default is `./.aura` — a relative path. The
        // prompt must absolutise it before the agent ever sees it,
        // otherwise the agent has no way to know which cwd the runtime
        // started from when constructing tool-call paths. The current env
        // block only prints `- Working directory:` (a subpath of the
        // workspace root, so absolutising it implicitly absolutises the
        // root) — the older `- Workspace root:` line was dropped in
        // commit 046c664 ("update env prompt").
        let workspace = WorkspaceManager::new(PathBuf::from("./relative-soul-test/.aura"));
        let soul = Soul::from_workspace(&workspace).await.expect("soul");
        let prompt = soul.system_prompt(TEST_SESSION_ID);

        let mut saw_work_dir = false;
        for line in prompt.lines() {
            if let Some(value) = line.strip_prefix("- Working directory: ") {
                assert!(
                    value.starts_with('/'),
                    "Working directory must be absolute: {line}"
                );
                assert!(
                    !value.contains("/./"),
                    "Working directory must not carry `.` segments: {line}"
                );
                saw_work_dir = true;
            }
        }
        assert!(saw_work_dir, "no `- Working directory:` line in: {prompt}");
    }

    #[tokio::test]
    async fn from_workspace_appends_env_block_after_identity_files() {
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
        let prompt = soul.system_prompt(TEST_SESSION_ID);

        let env_pos = prompt.find("# Environment").expect("env block present");
        let soul_pos = prompt.find("I am thoughtful.").expect("soul text present");
        let identity_pos = prompt.find("Name: Aura.").expect("identity text present");
        let hint_pos = prompt
            .find("You are an intelligent AI assistant.")
            .expect("top hint present");
        assert_eq!(hint_pos, 0, "top hint must be at offset 0: {prompt}");
        assert!(
            hint_pos < soul_pos && soul_pos < identity_pos && identity_pos < env_pos,
            "expected hint < soul < identity < env, got positions hint={hint_pos} soul={soul_pos} identity={identity_pos} env={env_pos}"
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
    fn custom_does_not_inject_env_block() {
        let soul = Soul::custom("just this".to_string());
        assert_eq!(soul.system_prompt(TEST_SESSION_ID), "just this");
    }

    #[tokio::test]
    async fn from_workspace_substitutes_session_id_in_env_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceManager::new(dir.path().to_path_buf());
        let soul = Soul::from_workspace(&workspace).await.expect("soul");

        let prompt = soul.system_prompt("sess-abc-42");
        assert!(
            prompt.contains("- Session ID: sess-abc-42"),
            "session id should be substituted into env block: {prompt}"
        );
        assert!(
            !prompt.contains(SESSION_ID_PLACEHOLDER),
            "raw placeholder must not survive substitution: {prompt}"
        );

        let other = soul.system_prompt("sess-xyz");
        assert!(other.contains("- Session ID: sess-xyz"));
    }
}
