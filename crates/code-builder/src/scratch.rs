use std::path::{Path, PathBuf};

use aura_workspace::WorkspacePaths;
use uuid::Uuid;

use crate::error::CodeBuilderError;

/// Per-call run directory under `<workspace>/.aura/code-builder/runs/<uuid>/`.
///
/// Holds:
/// - `script.py` — persisted; the agent's outer LLM sees this path in
///   the tool result, not the inlined code.
/// - `stdout.txt` / `stderr.txt` — persisted **only** when the live
///   output exceeded the inline threshold; written by the tool after
///   leak-pattern sanitization.
/// - `uv-cache/`, `workdir/` — ephemeral; `keep()` + `Drop` trim them
///   after a successful run.
///
/// Failure path: if `keep()` was never called, `Drop` removes the entire
/// run directory so failed runs don't leak partial state into the
/// workspace.
pub(crate) struct RunDir {
    pub root: PathBuf,
    pub script_path: PathBuf,
    pub uv_cache_dir: PathBuf,
    pub workdir: PathBuf,
    keep_artifacts: bool,
}

impl RunDir {
    pub fn create(workspace_root: &Path) -> Result<Self, CodeBuilderError> {
        let base = WorkspacePaths::new(workspace_root.to_path_buf()).code_builder_runs_dir();
        let id = Uuid::new_v4();
        let root = base.join(id.to_string());
        let uv_cache_dir = root.join("uv-cache");
        let workdir = root.join("workdir");
        let script_path = root.join("script.py");

        std::fs::create_dir_all(&uv_cache_dir).map_err(scratch_err)?;
        std::fs::create_dir_all(&workdir).map_err(scratch_err)?;

        Ok(Self {
            root,
            script_path,
            uv_cache_dir,
            workdir,
            keep_artifacts: false,
        })
    }

    pub fn write_script(&self, code: &str) -> Result<(), CodeBuilderError> {
        std::fs::write(&self.script_path, code).map_err(scratch_err)
    }

    pub fn stdout_path(&self) -> PathBuf {
        self.root.join("stdout.txt")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.root.join("stderr.txt")
    }

    /// Write a long stdout/stderr body to disk with mode 0600. The body
    /// must already be sanitised against leak patterns by the caller.
    pub fn write_overflow(&self, path: &Path, body: &str) -> Result<(), CodeBuilderError> {
        write_private_file(path, body.as_bytes()).map_err(scratch_err)
    }

    /// Mark the run as successful — `Drop` will only trim ephemeral
    /// subdirs (uv cache, workdir) and keep `script.py` + any overflow
    /// stdout/stderr files.
    pub fn keep(&mut self) {
        self.keep_artifacts = true;
    }
}

fn scratch_err(e: std::io::Error) -> CodeBuilderError {
    CodeBuilderError::Scratch(e.to_string())
}

fn write_private_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body)?;
    f.sync_all()?;
    Ok(())
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if !self.keep_artifacts {
            let _ = std::fs::remove_dir_all(&self.root);
            return;
        }
        let _ = std::fs::remove_dir_all(&self.uv_cache_dir);
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_makes_subdirs_and_drop_removes_tree_when_not_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let root_clone;
        {
            let s = RunDir::create(tmp.path()).unwrap();
            assert!(s.uv_cache_dir.exists());
            assert!(s.workdir.exists());
            assert!(!s.script_path.exists());
            root_clone = s.root.clone();
        }
        assert!(
            !root_clone.exists(),
            "run dir should be removed on drop when keep() not called"
        );
    }

    #[test]
    fn keep_then_drop_trims_ephemerals_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root_clone;
        let script_path;
        {
            let mut s = RunDir::create(tmp.path()).unwrap();
            s.write_script("print('hi')").unwrap();
            s.keep();
            root_clone = s.root.clone();
            script_path = s.script_path.clone();
        }
        assert!(
            script_path.exists(),
            "script.py must survive Drop when keep() was called"
        );
        assert!(root_clone.exists(), "run root must survive Drop");
        // uv-cache, workdir gone
        assert!(!root_clone.join("uv-cache").exists());
        assert!(!root_clone.join("workdir").exists());
    }

    #[test]
    fn each_create_uses_unique_root() {
        let tmp = tempfile::tempdir().unwrap();
        let a = RunDir::create(tmp.path()).unwrap();
        let b = RunDir::create(tmp.path()).unwrap();
        assert_ne!(a.root, b.root);
    }

    #[test]
    fn write_overflow_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let s = RunDir::create(tmp.path()).unwrap();
        let path = s.stdout_path();
        s.write_overflow(&path, "long body").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
