use std::path::{Path, PathBuf};

use baybo_model::AgentProfileId;
use baybo_workspace::WorkspacePaths;

use crate::ToolError;

/// The workspace-internal directories a shell command may reach, beyond
/// `work/`.
///
/// **One list, two gates.** A path inside the workspace has to clear both to
/// be usable from `Bash`, and they live at different layers:
///
/// 1. `BashTool` walks the command string and rejects any absolute token
///    inside the workspace but outside `work/` — before a process exists.
/// 2. The agent runtime binds these read-only over the `$BAYBO_HOME` tmpfs
///    mask when it builds the sandbox.
///
/// The tool-layer check runs first, so the two fail in opposite and equally
/// silent ways: exempted but unbound resolves inside the empty mask
/// (`No such file or directory`), bound but unexempted is refused on the
/// string and the mount is never reached. They were once two lists and drifted
/// exactly that way, which is why this function exists rather than a comment
/// asking the next editor to remember.
///
/// - the caller's **own** `skills/`, so an installed skill's bundled scripts
///   run in place. Deliberately not every agent's: binding a second one would
///   hand this session read access the scoped registry exists to deny.
/// - `state/blobs/`, so a `GetBlob` path can be handed to the program that
///   consumes it.
///
/// Read-only is the sandbox's job. The tool-layer check is lexical and cannot
/// tell a read from a write, so under `permission = free` — no sandbox at all
/// — both roots are writable. That is the trade the `skills/` exemption has
/// always made.
pub fn shell_reachable_workspace_roots(
    paths: &WorkspacePaths,
    agent: &AgentProfileId,
) -> Vec<PathBuf> {
    vec![agent.skills_dir(paths), paths.blobs_dir()]
}

pub(super) fn require_absolute(path: &Path, tool: &str, field: &str) -> crate::Result<()> {
    if !path.is_absolute() {
        return Err(ToolError::InvalidParams(format!(
            "{tool} `{field}` must be an absolute path, got `{}`",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn require_absolute_accepts_absolute() {
        assert!(require_absolute(&PathBuf::from("/tmp/x"), "T", "p").is_ok());
    }

    #[test]
    fn require_absolute_rejects_relative() {
        let err = require_absolute(&PathBuf::from("rel/x"), "T", "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")));
    }

    /// The list is the whole cross-layer contract, so what is *absent* from
    /// it matters as much as what is present — `state/blobs/` is a child of
    /// `state/`, and binding the parent would expose `storage.db` (every
    /// transcript, the secrets rows, every blob's read_token) and the
    /// browser profile's cookies to any shell command.
    ///
    /// Nothing downstream fails if the wrong entry appears: the session
    /// simply gains reach it should not have. This is the only place that
    /// catches it.
    #[test]
    fn shell_roots_are_the_callers_own_skills_and_the_blob_tree() {
        let paths = WorkspacePaths::new(PathBuf::from("/ws"));
        let mine = AgentProfileId::parse("01JMINE").expect("valid id");
        let theirs = AgentProfileId::parse("01JTHEIRS").expect("valid id");

        assert_eq!(
            shell_reachable_workspace_roots(&paths, &mine),
            vec![paths.persona_skills_dir("01JMINE"), paths.blobs_dir()],
        );
        assert!(
            !shell_reachable_workspace_roots(&paths, &mine)
                .contains(&paths.persona_skills_dir("01JTHEIRS")),
            "another agent's directory must never be reachable",
        );
        // The built-in is not a special case: it has a directory like anyone
        // else, and gets that one rather than a workspace-wide tree.
        assert_eq!(
            shell_reachable_workspace_roots(&paths, &AgentProfileId::builtin()),
            vec![
                paths.persona_skills_dir(baybo_workspace::paths::BUILTIN_PERSONA_DIR),
                paths.blobs_dir(),
            ],
        );
        assert_ne!(
            shell_reachable_workspace_roots(&paths, &theirs),
            shell_reachable_workspace_roots(&paths, &mine),
        );

        let roots = shell_reachable_workspace_roots(&paths, &mine);
        assert!(!roots.contains(&paths.state_dir()), "{roots:?}");
        assert!(!roots.contains(&paths.root().to_path_buf()), "{roots:?}");
    }
}
