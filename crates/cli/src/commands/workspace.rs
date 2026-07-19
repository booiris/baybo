//! `baybo workspace gc` — report and reclaim accumulated scratch under
//! `<workspace>/work`, the agent's only writable surface.
//!
//! Report-only by default: top-level `work/` entries are classified into
//! git clones, stale entries (newest in-tree mtime older than `--days`),
//! empty directories, and the contents of the disposable `work/tmp` dir
//! (first matching category wins). `--apply` deletes per category after
//! a y/N confirmation (`--yes` skips the prompts). Nothing is ever
//! followed through a symlink — links are measured and removed as links.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use baybo_model::ExternalAgentKind;
use baybo_workspace::WorkspacePaths;
use baybo_workspace::paths::{
    BROWSER_FONTS_SUBDIR, TOOL_SPILLS_SUBDIR, UV_STATE_SUBDIR, WORK_TMP_SUBDIR,
};
use serde_json::json;

use crate::cli::WorkspaceCmd;
use crate::commands::prompt;
use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

/// Protected `work/` entries without a `baybo-workspace` const or an
/// [`ExternalAgentKind`] root: `.claude` / `.claude.json` / `.codex` are
/// the external CLIs' HOME-level state, `runtime` and `deck-cards` hold
/// live agent-authored state.
const PROTECTED_LOCAL_NAMES: &[&str] =
    &[".claude", ".claude.json", ".codex", "runtime", "deck-cards"];

const SECS_PER_DAY: u64 = 86_400;

fn is_protected(name: &str) -> bool {
    if PROTECTED_LOCAL_NAMES.contains(&name) {
        return true;
    }
    if [
        UV_STATE_SUBDIR,
        BROWSER_FONTS_SUBDIR,
        TOOL_SPILLS_SUBDIR,
        WORK_TMP_SUBDIR,
    ]
    .contains(&name)
    {
        return true;
    }
    ExternalAgentKind::ALL.iter().any(|k| k.as_str() == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcCategory {
    GitClone,
    Stale,
    EmptyDir,
    TmpContents,
}

impl GcCategory {
    const ALL: [GcCategory; 4] = [
        GcCategory::GitClone,
        GcCategory::Stale,
        GcCategory::EmptyDir,
        GcCategory::TmpContents,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::GitClone => "git-clone",
            Self::Stale => "stale",
            Self::EmptyDir => "empty-dir",
            Self::TmpContents => "tmp",
        }
    }
}

#[derive(Debug)]
struct GcEntry {
    path: PathBuf,
    display_name: String,
    category: GcCategory,
    bytes: u64,
    newest_mtime: SystemTime,
    is_dir: bool,
}

pub async fn handle(ctx: &CommandContext, cmd: WorkspaceCmd) -> Result<CommandOutput> {
    match cmd {
        WorkspaceCmd::Gc { days, apply, yes } => gc(ctx, days, apply, yes),
    }
}

fn gc(ctx: &CommandContext, days: u64, apply: bool, yes: bool) -> Result<CommandOutput> {
    let paths = WorkspacePaths::new(ctx.workspace.root.clone());
    let work_dir = paths.work_dir();
    let now = SystemTime::now();
    let entries = scan(&work_dir, Duration::from_secs(days * SECS_PER_DAY), now);

    let report = render_report(&work_dir, &entries, now);
    let mut data = report_data(&work_dir, days, &entries, now);

    if !apply {
        return Ok(CommandOutput {
            human: report,
            data: Some(data),
        });
    }

    // The prompts sit between the report and the summary, so the report
    // goes straight to stdout instead of through `CommandOutput`.
    println!("{report}");
    let mut reclaimed: u64 = 0;
    let mut removed: usize = 0;
    for category in GcCategory::ALL {
        let batch: Vec<&GcEntry> = entries.iter().filter(|e| e.category == category).collect();
        if batch.is_empty() {
            continue;
        }
        let batch_bytes: u64 = batch.iter().map(|e| e.bytes).sum();
        if !yes {
            let question = format!(
                "Delete {} `{}` entr{} ({})?",
                batch.len(),
                category.label(),
                if batch.len() == 1 { "y" } else { "ies" },
                format_bytes(batch_bytes),
            );
            if !prompt::confirm(&question)? {
                continue;
            }
        }
        for entry in batch {
            match delete_entry(entry) {
                Ok(()) => {
                    reclaimed += entry.bytes;
                    removed += 1;
                }
                Err(e) => eprintln!("warning: failed to delete {}: {e}", entry.path.display()),
            }
        }
    }

    if let Some(map) = data.as_object_mut() {
        map.insert("applied".into(), json!(true));
        map.insert("removed_entries".into(), json!(removed));
        map.insert("reclaimed_bytes".into(), json!(reclaimed));
    }
    Ok(CommandOutput {
        human: format!(
            "Reclaimed {} across {removed} entr{}.",
            format_bytes(reclaimed),
            if removed == 1 { "y" } else { "ies" },
        ),
        data: Some(data),
    })
}

/// Classify the reclaimable entries under `work_dir`. Top-level entries
/// (protected names excluded) take the first matching category of
/// git-clone → stale → empty-dir; everything under `work/tmp` lands in
/// the `tmp` category unconditionally. Unreadable entries are skipped
/// with a stderr warning — the scan is best-effort, like the janitor.
fn scan(work_dir: &Path, stale_after: Duration, now: SystemTime) -> Vec<GcEntry> {
    let mut out = Vec::new();

    for (path, name) in list_dir(work_dir) {
        if is_protected(&name) {
            continue;
        }
        let Some((meta, bytes, newest)) = stat_entry(&path) else {
            continue;
        };
        let is_dir = meta.file_type().is_dir();
        let age = now.duration_since(newest).unwrap_or(Duration::ZERO);
        let category = if is_dir && std::fs::symlink_metadata(path.join(".git")).is_ok() {
            GcCategory::GitClone
        } else if age >= stale_after {
            GcCategory::Stale
        } else if is_dir && dir_is_empty(&path) {
            GcCategory::EmptyDir
        } else {
            continue;
        };
        out.push(GcEntry {
            path,
            display_name: name,
            category,
            bytes,
            newest_mtime: newest,
            is_dir,
        });
    }

    for (path, name) in list_dir(&work_dir.join(WORK_TMP_SUBDIR)) {
        let Some((meta, bytes, newest)) = stat_entry(&path) else {
            continue;
        };
        out.push(GcEntry {
            path,
            display_name: format!("{WORK_TMP_SUBDIR}/{name}"),
            category: GcCategory::TmpContents,
            bytes,
            newest_mtime: newest,
            is_dir: meta.file_type().is_dir(),
        });
    }

    out.sort_by(|a, b| {
        let ord = |c: GcCategory| GcCategory::ALL.iter().position(|x| *x == c);
        ord(a.category)
            .cmp(&ord(b.category))
            .then(b.bytes.cmp(&a.bytes))
            .then(a.display_name.cmp(&b.display_name))
    });
    out
}

/// Top-level `(path, display name)` pairs of `dir`; a missing dir is an
/// empty listing.
fn list_dir(dir: &Path) -> Vec<(PathBuf, String)> {
    let reader = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("warning: cannot read {}: {e}", dir.display());
            return Vec::new();
        }
    };
    reader
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (entry.path(), name)
        })
        .collect()
}

fn stat_entry(path: &Path) -> Option<(std::fs::Metadata, u64, SystemTime)> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("warning: cannot stat {}: {e}", path.display());
            return None;
        }
    };
    match walk_stats(path, &meta) {
        Ok((bytes, newest)) => Some((meta, bytes, newest)),
        Err(e) => {
            eprintln!("warning: cannot scan {}: {e}", path.display());
            None
        }
    }
}

/// du-style totals for one entry: summed lstat file sizes plus the
/// newest lstat mtime anywhere in the tree (directory mtimes included).
/// Symlinks contribute their own link metadata and are never followed.
fn walk_stats(root: &Path, meta: &std::fs::Metadata) -> std::io::Result<(u64, SystemTime)> {
    let mut newest = meta.modified()?;
    if !meta.file_type().is_dir() {
        return Ok((meta.len(), newest));
    }
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.metadata()?;
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
    Ok((bytes, newest))
}

fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut r| r.next().is_none())
        .unwrap_or(false)
}

fn delete_entry(entry: &GcEntry) -> std::io::Result<()> {
    if entry.is_dir {
        std::fs::remove_dir_all(&entry.path)
    } else {
        std::fs::remove_file(&entry.path)
    }
}

fn render_report(work_dir: &Path, entries: &[GcEntry], now: SystemTime) -> String {
    if entries.is_empty() {
        return format!("Nothing reclaimable under {}.", work_dir.display());
    }
    let name_width = entries
        .iter()
        .map(|e| e.display_name.chars().count())
        .max()
        .unwrap_or(0)
        .max("ENTRY".len());
    let mut lines = Vec::with_capacity(entries.len() + 4);
    lines.push(format!(
        "{:<name_width$}  {:<9}  {:>10}  {:>6}",
        "ENTRY", "CATEGORY", "SIZE", "AGE"
    ));
    for e in entries {
        let age = now.duration_since(e.newest_mtime).unwrap_or(Duration::ZERO);
        lines.push(format!(
            "{:<name_width$}  {:<9}  {:>10}  {:>6}",
            e.display_name,
            e.category.label(),
            format_bytes(e.bytes),
            format_age(age),
        ));
    }
    lines.push(String::new());
    let mut totals = Vec::new();
    for category in GcCategory::ALL {
        let batch: Vec<&GcEntry> = entries.iter().filter(|e| e.category == category).collect();
        if batch.is_empty() {
            continue;
        }
        let bytes: u64 = batch.iter().map(|e| e.bytes).sum();
        totals.push(format!(
            "{} {} ({})",
            batch.len(),
            category.label(),
            format_bytes(bytes)
        ));
    }
    lines.push(format!("Per category: {}", totals.join(" · ")));
    let grand: u64 = entries.iter().map(|e| e.bytes).sum();
    lines.push(format!(
        "Total: {} entries, {} under {}",
        entries.len(),
        format_bytes(grand),
        work_dir.display(),
    ));
    lines.join("\n")
}

fn report_data(
    work_dir: &Path,
    days: u64,
    entries: &[GcEntry],
    now: SystemTime,
) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let age = now.duration_since(e.newest_mtime).unwrap_or(Duration::ZERO);
            json!({
                "entry": e.display_name,
                "category": e.category.label(),
                "bytes": e.bytes,
                "age_days": age.as_secs() / SECS_PER_DAY,
            })
        })
        .collect();
    let mut totals = serde_json::Map::new();
    for category in GcCategory::ALL {
        let batch: Vec<&GcEntry> = entries.iter().filter(|e| e.category == category).collect();
        if batch.is_empty() {
            continue;
        }
        let bytes: u64 = batch.iter().map(|e| e.bytes).sum();
        totals.insert(
            category.label().to_string(),
            json!({"entries": batch.len(), "bytes": bytes}),
        );
    }
    json!({
        "work_dir": work_dir.display().to_string(),
        "days": days,
        "entries": rows,
        "totals": totals,
        "total_entries": entries.len(),
        "total_bytes": entries.iter().map(|e| e.bytes).sum::<u64>(),
    })
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_age(age: Duration) -> String {
    let days = age.as_secs() / SECS_PER_DAY;
    if days == 0 {
        "<1d".to_string()
    } else {
        format!("{days}d")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    const STALE_AFTER: Duration = Duration::from_secs(14 * SECS_PER_DAY);
    const OVER_STALE: Duration = Duration::from_secs(15 * SECS_PER_DAY);

    fn back_date(path: &Path, age: Duration) {
        let mtime = SystemTime::now() - age;
        let f = std::fs::File::open(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    fn back_date_tree(root: &Path, age: Duration) {
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

    fn back_date_symlink(path: &Path, age: Duration) {
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

    fn category_of<'a>(entries: &'a [GcEntry], name: &str) -> Option<&'a GcEntry> {
        entries.iter().find(|e| e.display_name == name)
    }

    #[test]
    fn scan_classifies_by_first_matching_category() {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path();

        // Git clone — stale too, but git-clone wins as the first match.
        let clone = work.join("some-repo");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::write(clone.join(".git").join("HEAD"), b"ref").unwrap();
        back_date_tree(&clone, OVER_STALE);

        // Stale plain dir.
        let old_dir = work.join("old-scratch");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("out.txt"), b"12345").unwrap();
        back_date_tree(&old_dir, OVER_STALE);

        // Fresh non-empty dir — not reclaimable, not listed.
        let fresh_dir = work.join("fresh-project");
        std::fs::create_dir_all(&fresh_dir).unwrap();
        std::fs::write(fresh_dir.join("keep.txt"), b"x").unwrap();

        // Fresh empty dir.
        std::fs::create_dir_all(work.join("empty-dir")).unwrap();

        // Stale top-level file.
        let old_file = work.join("dump.sql");
        std::fs::write(&old_file, b"data!").unwrap();
        back_date(&old_file, OVER_STALE);

        // tmp/ contents are category 4 even when fresh.
        std::fs::create_dir_all(work.join(WORK_TMP_SUBDIR)).unwrap();
        std::fs::write(work.join(WORK_TMP_SUBDIR).join("probe.py"), b"x").unwrap();

        let entries = scan(work, STALE_AFTER, SystemTime::now());

        assert_eq!(
            category_of(&entries, "some-repo").unwrap().category,
            GcCategory::GitClone
        );
        assert_eq!(
            category_of(&entries, "old-scratch").unwrap().category,
            GcCategory::Stale
        );
        assert_eq!(
            category_of(&entries, "dump.sql").unwrap().category,
            GcCategory::Stale
        );
        assert_eq!(
            category_of(&entries, "empty-dir").unwrap().category,
            GcCategory::EmptyDir
        );
        assert_eq!(
            category_of(&entries, "tmp/probe.py").unwrap().category,
            GcCategory::TmpContents
        );
        assert!(
            category_of(&entries, "fresh-project").is_none(),
            "fresh non-empty dirs are kept off the report"
        );
        assert_eq!(entries.len(), 5);

        let old = category_of(&entries, "old-scratch").unwrap();
        assert_eq!(old.bytes, 5, "size sums contained file lengths");
    }

    #[test]
    fn scan_never_lists_protected_names() {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path();
        let mut protected: Vec<String> = [
            UV_STATE_SUBDIR,
            BROWSER_FONTS_SUBDIR,
            TOOL_SPILLS_SUBDIR,
            "runtime",
            "deck-cards",
            ".claude",
            ".codex",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for kind in ExternalAgentKind::ALL {
            protected.push(kind.as_str().to_string());
        }
        for name in &protected {
            let dir = work.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("state.bin"), b"x").unwrap();
            back_date_tree(&dir, OVER_STALE);
        }
        // `.claude.json` is a file, not a dir.
        let claude_json = work.join(".claude.json");
        std::fs::write(&claude_json, b"{}").unwrap();
        back_date(&claude_json, OVER_STALE);
        // An empty `tmp/` contributes nothing either.
        std::fs::create_dir_all(work.join(WORK_TMP_SUBDIR)).unwrap();

        let entries = scan(work, STALE_AFTER, SystemTime::now());
        assert!(
            entries.is_empty(),
            "protected entries leaked into the report: {entries:?}"
        );
    }

    #[test]
    fn scan_measures_symlinks_without_following_them() {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        // Outside target with a fresh file: following any link into it
        // would flip staleness or inflate sizes.
        let target = tmp.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("fresh.bin"), vec![0u8; 4096]).unwrap();

        // Fresh top-level symlink → not reclaimable, not listed.
        let fresh_link = work.join("fresh-link");
        std::os::unix::fs::symlink(&target, &fresh_link).unwrap();

        // Stale top-level symlink → stale by its own lstat mtime.
        let stale_link = work.join("stale-link");
        std::os::unix::fs::symlink(&target, &stale_link).unwrap();
        back_date_symlink(&stale_link, OVER_STALE);

        // Stale dir with a stale inner link to the fresh target: stays
        // stale only if the walk refuses to traverse the link.
        let dir_with_link = work.join("old-with-link");
        std::fs::create_dir_all(&dir_with_link).unwrap();
        let inner = dir_with_link.join("link");
        std::os::unix::fs::symlink(&target, &inner).unwrap();
        back_date_symlink(&inner, OVER_STALE);
        back_date_tree(&dir_with_link, OVER_STALE);

        let entries = scan(&work, STALE_AFTER, SystemTime::now());

        assert!(category_of(&entries, "fresh-link").is_none());
        let stale = category_of(&entries, "stale-link").unwrap();
        assert_eq!(stale.category, GcCategory::Stale);
        assert!(!stale.is_dir, "a symlink is deleted as a link, not a dir");
        let dir = category_of(&entries, "old-with-link").unwrap();
        assert_eq!(dir.category, GcCategory::Stale);
        assert!(
            dir.bytes < 4096,
            "walk must not count the link target's bytes, got {}",
            dir.bytes
        );

        // Deleting both stale entries must leave the target intact.
        delete_entry(stale).unwrap();
        delete_entry(dir).unwrap();
        assert!(std::fs::symlink_metadata(&stale_link).is_err());
        assert!(!dir_with_link.exists());
        assert!(target.join("fresh.bin").exists());
    }
}
