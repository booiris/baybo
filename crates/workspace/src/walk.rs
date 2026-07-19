//! Sync directory-tree measurement shared by the janitor's `work/tmp`
//! sweep and the `baybo workspace gc` scan. One walker so the two
//! staleness gates can never disagree about what "newest mtime" means.

use std::path::Path;
use std::time::SystemTime;

/// du-style totals for one filesystem entry, as measured by
/// [`tree_stats`].
#[derive(Debug, Clone, Copy)]
pub struct TreeStats {
    /// Summed lstat sizes of every non-directory in the tree (a symlink
    /// contributes the link's own length, never its target's).
    pub bytes: u64,
    /// Newest lstat mtime anywhere in the tree: the entry's own mtime
    /// plus — for a real directory — the lstat mtime of everything
    /// beneath it, directories included (so a create or delete inside
    /// refreshes its parent).
    pub newest_mtime: SystemTime,
}

/// Measure `path` without ever following a symlink: the entry itself is
/// lstat'ed, a symlink is measured as the link, and links inside a
/// directory never pull outside trees into the result. Entries that
/// vanish mid-walk are skipped; any other I/O error is returned. Sync
/// on purpose — async callers wrap it in `tokio::task::spawn_blocking`.
pub fn tree_stats(path: &Path) -> std::io::Result<TreeStats> {
    let meta = std::fs::symlink_metadata(path)?;
    let mut newest = meta.modified()?;
    if !meta.file_type().is_dir() {
        return Ok(TreeStats {
            bytes: meta.len(),
            newest_mtime: newest,
        });
    }
    let mut bytes = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            // `DirEntry::metadata` is lstat-shaped: it never traverses
            // the entry when it is a symlink.
            let m = match entry.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if let Ok(t) = m.modified()
                && t > newest
            {
                newest = t;
            }
            if m.file_type().is_dir() {
                stack.push(entry.path());
            } else {
                bytes += m.len();
            }
        }
    }
    Ok(TreeStats {
        bytes,
        newest_mtime: newest,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::{back_date_symlink, back_date_tree};

    #[test]
    fn tree_stats_sums_files_and_takes_the_newest_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("entry");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("a.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(root.join("nested").join("b.bin"), vec![0u8; 5]).unwrap();
        back_date_tree(&root, Duration::from_secs(1000));
        std::fs::write(root.join("nested").join("fresh.bin"), vec![0u8; 3]).unwrap();

        let stats = tree_stats(&root).unwrap();
        assert_eq!(stats.bytes, 18);
        let age = SystemTime::now()
            .duration_since(stats.newest_mtime)
            .unwrap();
        assert!(
            age < Duration::from_secs(100),
            "one fresh file deep in the tree must win the newest read"
        );
    }

    #[test]
    fn tree_stats_never_follows_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("fresh.bin"), vec![0u8; 4096]).unwrap();

        let root = tmp.path().join("entry");
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        back_date_symlink(&link, Duration::from_secs(1000));
        back_date_tree(&root, Duration::from_secs(1000));

        let stats = tree_stats(&root).unwrap();
        assert!(
            stats.bytes < 4096,
            "link target bytes leaked into the sum: {}",
            stats.bytes
        );
        let age = SystemTime::now()
            .duration_since(stats.newest_mtime)
            .unwrap();
        assert!(
            age > Duration::from_secs(500),
            "the fresh link target must not refresh the tree"
        );

        // A top-level symlink is measured as the link itself.
        let solo = tree_stats(&link).unwrap();
        assert!(solo.bytes < 4096);
    }
}
