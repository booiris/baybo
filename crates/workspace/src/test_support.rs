//! Test-only mtime back-dating helpers for code that gates on
//! [`crate::walk::tree_stats`]-style newest-mtime reads (the janitor
//! sweeps). Gated behind `test-support` so nothing
//! here ships in a release build; downstream crates pull it in with
//! `baybo-workspace = { workspace = true, features = ["test-support"] }`
//! in their `[dev-dependencies]`.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Backdate one file's mtime by `age`.
pub fn back_date(path: &Path, age: Duration) {
    let mtime = SystemTime::now() - age;
    let f = std::fs::File::open(path).unwrap();
    f.set_modified(mtime).unwrap();
}

/// Backdate every entry in the tree (files, dirs, and the root itself)
/// so a newest-in-tree-mtime read sees the whole entry as `age` old.
pub fn back_date_tree(root: &Path, age: Duration) {
    let mtime = SystemTime::now() - age;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(p.clone());
            }
            seen.push(p);
        }
        seen.push(dir);
    }
    for p in seen {
        let f = std::fs::File::open(&p).unwrap();
        let _ = f.set_modified(mtime);
    }
}

/// Backdate a symlink's *own* lstat mtime. `File::open` follows the
/// link, so this goes through `utimensat(AT_SYMLINK_NOFOLLOW)`.
pub fn back_date_symlink(path: &Path, age: Duration) {
    use std::os::unix::ffi::OsStrExt;
    let mtime = SystemTime::now() - age;
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let ts = libc::timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    let times = [ts, ts];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "utimensat failed for {}", path.display());
}
