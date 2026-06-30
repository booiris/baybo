use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymlinkMount {
    source: PathBuf,
    destination: PathBuf,
}

impl WorkspaceSymlinkMount {
    pub(crate) fn new(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
        }
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

pub fn workspace_symlink_mount_for(
    requested_path: &Path,
    workspace_root: &Path,
) -> Option<WorkspaceSymlinkMount> {
    if !requested_path.is_absolute() {
        return None;
    }
    let canonical_root = workspace_root.canonicalize().ok()?;
    let canonical_path = requested_path.canonicalize().ok()?;
    if !canonical_path.starts_with(&canonical_root) {
        return None;
    }
    if canonical_path == requested_path {
        return None;
    }
    Some(WorkspaceSymlinkMount::new(
        canonical_path,
        requested_path.to_path_buf(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn returns_mount_for_symlinked_workspace_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Canonicalize the temp root up front (as `empty_when_path_is_already_canonical`
        // does) so the path comparisons hold where the temp dir itself sits under a
        // symlink — on macOS `$TMPDIR` is `/var/folders/…` and `/var` → `/private/var`.
        // `mount.source()` is the canonicalized requested path, so the expected `work`
        // must be canonical too, or it mismatches on `/var` vs `/private/var` alone.
        let root = tmp.path().canonicalize().expect("canonicalize tmp");
        let real = root.join("real");
        let work = real.join("work");
        std::fs::create_dir_all(&work).expect("real/work");
        let symlink_root = root.join("symlink");
        std::os::unix::fs::symlink(&real, &symlink_root).expect("symlink");

        let mount = workspace_symlink_mount_for(&symlink_root.join("work"), &real)
            .expect("symlinked path inside workspace should produce mount");
        assert_eq!(mount.source(), work.as_path());
        assert_eq!(mount.destination(), symlink_root.join("work").as_path());
    }

    #[test]
    fn empty_when_path_is_already_canonical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().canonicalize().expect("canonicalize");
        let work = real.join("work");
        std::fs::create_dir_all(&work).expect("work");

        let mount = workspace_symlink_mount_for(&work, &real);
        assert!(mount.is_none(), "got {mount:?}");
    }

    #[test]
    fn empty_when_symlink_resolves_outside_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&real).expect("real");
        std::fs::create_dir_all(&other).expect("other");
        let symlink_to_other = tmp.path().join("symlink-other");
        std::os::unix::fs::symlink(&other, &symlink_to_other).expect("symlink");

        let mount = workspace_symlink_mount_for(&symlink_to_other, &real);
        assert!(
            mount.is_none(),
            "symlink resolves outside workspace; got {mount:?}"
        );
    }

    #[test]
    fn empty_when_requested_path_is_relative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mount = workspace_symlink_mount_for(Path::new("relative/path"), tmp.path());
        assert!(mount.is_none(), "got {mount:?}");
    }
}
