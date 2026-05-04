use std::path::{Path, PathBuf};

use aura_workspace::{WorkspaceManager, WorkspacePaths};
use tracing::debug;

/// The Soul system loads personality and identity from workspace files
/// and produces the system prompt for LLM conversations.
pub struct Soul {
    system_prompt: String,
}

impl Soul {
    /// Build a Soul from workspace identity files. Always prepends an
    /// environment block describing the workspace layout — the LLM
    /// uses this to construct absolute paths inside the working
    /// directory so tool calls land where the OS sandbox is rooted.
    pub async fn from_workspace(workspace: &WorkspaceManager) -> anyhow::Result<Self> {
        let identity = workspace.load_identity_files().await?;
        let mut parts = vec![build_env_block(workspace)];

        if let Some(soul_text) = &identity.soul {
            parts.push(soul_text.clone());
        }
        if let Some(identity_text) = &identity.identity {
            parts.push(identity_text.clone());
        }
        if let Some(agents_text) = &identity.agents {
            parts.push(agents_text.clone());
        }

        if parts.len() == 1 {
            // Only the env block — no identity files were present. Add
            // a minimal default so the prompt isn't pure-environment.
            parts.push("You are Aura, an intelligent assistant.".to_string());
        }

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

    /// Get the system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

/// Render the agent-facing environment block. Mirrors what `runtime.rs`
/// hands the tool layer: `ToolContext.workspace_root` is the workspace
/// `work/` subdirectory (also the OS sandbox FS scope), and the
/// surrounding `<root>` carries identity / state / logs. Both paths
/// are absolutised before rendering — a relative workspace root (e.g.
/// the debug-build default `./.aura`) would otherwise leak a
/// cwd-relative path into the prompt and the agent has no way to know
/// what cwd the runtime started from.
fn build_env_block(workspace: &WorkspaceManager) -> String {
    let paths = WorkspacePaths::new(workspace.root.clone());
    let work_dir = absolutise(&paths.work_dir());
    format!(
        "# Environment\n\
         - Working directory: {work_dir}\n\
         - Platform: {platform}\n\
         \n\
         Tool calls operate inside the working directory by default — \
         `Bash` spawns from there, the OS sandbox restricts writes to \
         that subtree, and path-accepting tools (`Read`, `Edit`, \
         `Write`, `Glob`, `Grep`) expect absolute paths. Construct \
         absolute paths under the working directory to keep operations \
         inside the sandbox; reach outside (e.g. read `/etc/hosts`) \
         only when the task explicitly calls for it.",
        work_dir = work_dir.display(),
        platform = std::env::consts::OS,
    )
}

/// Best-effort path absolutisation. Prefers `canonicalize` (resolves
/// symlinks too — matches the form `runtime.rs` hands the OS sandbox)
/// and falls back to `std::path::absolute` + `.`-segment stripping when
/// the path doesn't yet exist on disk (e.g. boot before
/// `ensure_layout`, or unit tests pointing at a freshly-named
/// tempdir). `std::path::absolute` joins relative paths with cwd but
/// does not normalise `.` components — strip them manually so the
/// prompt doesn't show `<cwd>/./.aura/work`. `..` is left intact; the
/// OS resolves it correctly on access and proper normalisation
/// requires a real filesystem walk.
fn absolutise(p: &Path) -> PathBuf {
    if let Ok(canonical) = p.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut cleaned = PathBuf::new();
    for component in absolute.components() {
        if !matches!(component, std::path::Component::CurDir) {
            cleaned.push(component);
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_workspace::IdentityKind;

    #[tokio::test]
    async fn from_workspace_prepends_env_block_with_no_identity_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceManager::new(dir.path().to_path_buf());
        let soul = Soul::from_workspace(&workspace).await.expect("soul");

        let prompt = soul.system_prompt();
        let expected_work_dir =
            absolutise(&WorkspacePaths::new(dir.path().to_path_buf()).work_dir())
                .display()
                .to_string();
        let expected_workspace_root = absolutise(dir.path()).display().to_string();
        assert!(
            prompt.starts_with("# Environment"),
            "env block must come first: {prompt}"
        );
        assert!(
            prompt.contains(&expected_work_dir),
            "missing work_dir {expected_work_dir} in: {prompt}"
        );
        assert!(
            prompt.contains(&expected_workspace_root),
            "missing workspace root {expected_workspace_root} in: {prompt}"
        );
        assert!(
            prompt.contains(std::env::consts::OS),
            "missing platform in: {prompt}"
        );
        assert!(
            prompt.contains("You are Aura"),
            "fallback identity must follow env block when no identity files exist: {prompt}"
        );
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
        let prompt = soul.system_prompt();

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
    async fn from_workspace_keeps_identity_files_after_env_block() {
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

        let env_pos = prompt.find("# Environment").expect("env block present");
        let soul_pos = prompt.find("I am thoughtful.").expect("soul text present");
        let identity_pos = prompt.find("Name: Aura.").expect("identity text present");
        assert!(
            env_pos < soul_pos && soul_pos < identity_pos,
            "expected env < soul < identity, got positions env={env_pos} soul={soul_pos} identity={identity_pos}"
        );
        assert!(
            !prompt.contains("You are Aura, an intelligent assistant."),
            "fallback identity must NOT appear when real identity files load: {prompt}"
        );
    }

    #[test]
    fn custom_does_not_inject_env_block() {
        let soul = Soul::custom("just this".to_string());
        assert_eq!(soul.system_prompt(), "just this");
    }
}
