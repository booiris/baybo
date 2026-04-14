//! Metadata fingerprint of a skill directory.
//!
//! We used to SHA-256 every byte of every file. That gave bit-for-bit
//! guarantees but meant reading up to 512 KiB per file on every gate
//! call — for a 100 MB skill tree that's 100 ms+ of I/O on a path that
//! pretends to be non-blocking. The current scheme hashes
//! `(path, kind, size, mtime-ns)` per entry instead; the walk is
//! stat-only and typical edits still trip the hash because editors
//! bump mtime.
//!
//! **Tradeoff, explicitly**: an attacker who can `touch -t` a file
//! back to its prior mtime, or a filesystem with coarse mtime
//! resolution (HFS, some network FS), can in principle keep a
//! modified file indistinguishable from the prior version under this
//! scheme. The assessor's threat model already assumes the attacker
//! has some write access inside the workspace; defeating mtime
//! forgery too would require reading every byte, which is the cost
//! we just chose to stop paying. If this ever becomes a concrete
//! threat, swap `hash_metadata` back to a content hash.
//!
//! Because mtime is part of the fingerprint, the hash is now
//! **per-install**: two machines with bit-identical skill directories
//! will compute different hashes because their mtimes differ. That's
//! fine — the verdict cache is local-only.
//!
//! Stability rules preserved for same-install determinism:
//! - Entries sorted by relative path.
//! - Forward-slash paths so a cache written on Linux matches Windows.
//! - Length-prefixed rel-paths to close the variable-field aliasing
//!   hazard (`"foo" + "/bar"` vs `"foo/" + "bar"`).
//! - Scope discriminator prefix (`aura.skill.full:v1` vs
//!   `aura.skill.primary:v1`) so the two scopes never collide.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

/// Discriminators for the two hash scopes. Prefixing the hasher state
/// with a scope-specific tag ensures the same on-disk content never
/// collides across scopes — so a one-file skill can't have its
/// `SKILL.md`-only hash and its full-dir hash coincide.
const FULL_SCOPE_TAG: &[u8] = b"aura.skill.full:v1\n";
const PRIMARY_SCOPE_TAG: &[u8] = b"aura.skill.primary:v1\n";

/// The entrypoint file inspected by the primary-scope hash.
pub(crate) const PRIMARY_FILE: &str = "SKILL.md";

/// Directory-size thresholds. If either is exceeded, the assessor
/// switches to tiered mode: a fast primary-only verdict gates use,
/// and the full-scope check runs in the background.
const TIER_MAX_FILES: usize = 4;
const TIER_MAX_BYTES: u64 = 16 * 1024;

/// Directory-level hard-reject thresholds. Any skill directory whose
/// file count or aggregate byte size exceeds these caps is refused
/// outright — a legitimate skill is a prompt plus a handful of helper
/// files, so a tree bigger than this is either a misconfiguration or
/// an attempt to DoS the hash/LLM pipeline with garbage. Enforced by
/// [`hash_skill_dir`] before any hashing work starts.
const MAX_TOTAL_FILES: usize = 500;
const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

/// Compute a hex-encoded SHA-256 over the metadata fingerprint of `dir`.
///
/// See the module doc for why this is metadata-based rather than a
/// content hash.
pub fn hash_skill_dir(dir: &Path) -> io::Result<String> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", dir.display()),
        ));
    }

    enforce_limits(dir)?;

    let mut entries = Vec::new();
    collect_entries(dir, dir, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    hasher.update(FULL_SCOPE_TAG);
    for (rel_path, file_path, kind) in entries {
        // rel-path length prefix then bytes; length prefix prevents the
        // "foo" + "/bar" / "foo/" + "bar" aliasing hazard when two
        // adjacent fields are both variable-length strings.
        let rel_bytes = rel_path.as_bytes();
        hasher.update((rel_bytes.len() as u64).to_le_bytes());
        hasher.update(rel_bytes);
        hasher.update([kind.tag()]);

        match kind {
            EntryKind::File => {
                let file_meta = fs::metadata(&file_path)?;
                hash_metadata(&file_meta, &mut hasher)?;
            }
            EntryKind::Symlink => {
                let target = fs::read_link(&file_path)?;
                let bytes = target.to_string_lossy();
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(bytes.as_bytes());
            }
        }
    }

    Ok(hex::encode(hasher.finalize()))
}

/// Compute the primary-scope metadata hash — SKILL.md alone, ignoring
/// any helper files. Used as the fast-path cache key for tiered
/// assessment. Returns `Ok(None)` if `SKILL.md` is missing (skill is
/// inline or structurally broken; caller should fall through to the
/// full-scope flow).
pub fn hash_skill_primary(dir: &Path) -> io::Result<Option<String>> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", dir.display()),
        ));
    }

    let primary = dir.join(PRIMARY_FILE);
    match fs::symlink_metadata(&primary) {
        Ok(m) if m.is_file() => {
            let mut hasher = Sha256::new();
            hasher.update(PRIMARY_SCOPE_TAG);
            hash_metadata(&m, &mut hasher)?;
            Ok(Some(hex::encode(hasher.finalize())))
        }
        Ok(_) | Err(_) => Ok(None),
    }
}

/// Probe `dir` to decide whether the assessor should tier the check.
///
/// Returns `true` if either the file count or the aggregate byte size
/// exceeds the configured threshold. Symlinks count toward the file
/// total but their target bytes do not contribute.
pub fn should_tier(dir: &Path) -> io::Result<bool> {
    let mut files = 0usize;
    let mut bytes: u64 = 0;
    probe(dir, &mut files, &mut bytes)?;
    Ok(files > TIER_MAX_FILES || bytes > TIER_MAX_BYTES)
}

/// Reject trees that blow past the hard caps. Called up front in
/// [`hash_skill_dir`] so a pathological directory fails fast instead
/// of consuming I/O and memory during the hash walk.
fn enforce_limits(dir: &Path) -> io::Result<()> {
    let mut files = 0usize;
    let mut bytes: u64 = 0;
    probe(dir, &mut files, &mut bytes)?;
    if files > MAX_TOTAL_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill directory contains {files} files; hard limit is {MAX_TOTAL_FILES}"),
        ));
    }
    if bytes > MAX_TOTAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill directory is {bytes} bytes; hard limit is {MAX_TOTAL_BYTES}"),
        ));
    }
    Ok(())
}

fn probe(dir: &Path, files: &mut usize, bytes: &mut u64) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            probe(&path, files, bytes)?;
            continue;
        }
        if meta.file_type().is_symlink() {
            *files += 1;
            continue;
        }
        if meta.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(meta.len());
        }
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum EntryKind {
    File,
    Symlink,
}

impl EntryKind {
    fn tag(self) -> u8 {
        match self {
            EntryKind::File => b'f',
            EntryKind::Symlink => b's',
        }
    }
}

/// Depth-first walk collecting `(relative-path-with-forward-slashes, absolute-path, kind)`.
fn collect_entries(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf, EntryKind)>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;

        if meta.is_dir() {
            collect_entries(root, &path, out)?;
            continue;
        }

        let rel = match path.strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let rel_str = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");

        if meta.file_type().is_symlink() {
            out.push((rel_str, path, EntryKind::Symlink));
        } else if meta.is_file() {
            out.push((rel_str, path, EntryKind::File));
        }
    }
    Ok(())
}

/// Fold the size + modification time of a file into the hasher. This
/// is the whole "content" fingerprint — file bodies are deliberately
/// not read (see module doc for the rationale and tradeoff).
fn hash_metadata(meta: &fs::Metadata, hasher: &mut Sha256) -> io::Result<()> {
    hasher.update(meta.len().to_le_bytes());
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    hasher.update(mtime.as_secs().to_le_bytes());
    hasher.update(mtime.subsec_nanos().to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn hash_is_stable_for_unchanged_tree() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "body").unwrap();
        let h1 = hash_skill_dir(d.path()).unwrap();
        let h2 = hash_skill_dir(d.path()).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn size_change_changes_hash() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "hello").unwrap();
        let h1 = hash_skill_dir(d.path()).unwrap();

        // Different size → metadata differs → hash differs.
        fs::write(d.path().join("SKILL.md"), "hello!").unwrap();
        let h2 = hash_skill_dir(d.path()).unwrap();

        assert_ne!(h1, h2);
    }

    #[test]
    fn adding_supporting_file_changes_hash() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "main").unwrap();
        let h1 = hash_skill_dir(d.path()).unwrap();

        fs::create_dir(d.path().join("scripts")).unwrap();
        fs::write(d.path().join("scripts/helper.sh"), "echo hi").unwrap();
        let h2 = hash_skill_dir(d.path()).unwrap();

        assert_ne!(h1, h2);
    }

    #[test]
    fn rename_changes_hash_even_with_same_content() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        fs::write(a.path().join("SKILL.md"), "x").unwrap();
        fs::write(b.path().join("OTHER.md"), "x").unwrap();

        assert_ne!(
            hash_skill_dir(a.path()).unwrap(),
            hash_skill_dir(b.path()).unwrap()
        );
    }

    #[test]
    fn adjacent_files_cannot_alias_across_length_boundary() {
        // Regression: without a length prefix on the rel-path, a file
        // named "a" containing "bX" could hash the same as "ab" with
        // content "X". The length prefix guarantees they differ.
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        fs::write(a.path().join("a"), "bX").unwrap();
        fs::write(b.path().join("ab"), "X").unwrap();

        assert_ne!(
            hash_skill_dir(a.path()).unwrap(),
            hash_skill_dir(b.path()).unwrap()
        );
    }

    #[test]
    fn missing_dir_returns_error() {
        let d = tempdir().unwrap();
        let bogus = d.path().join("does-not-exist");
        assert!(hash_skill_dir(&bogus).is_err());
    }

    #[test]
    fn primary_hash_ignores_supporting_files() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "main body").unwrap();
        let p1 = hash_skill_primary(d.path()).unwrap().unwrap();

        fs::create_dir(d.path().join("scripts")).unwrap();
        fs::write(d.path().join("scripts/helper.sh"), "echo hi").unwrap();
        let p2 = hash_skill_primary(d.path()).unwrap().unwrap();

        assert_eq!(p1, p2, "primary scope must ignore helper files");
    }

    #[test]
    fn primary_and_full_hash_differ_even_for_single_file_skill() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "body").unwrap();
        let primary = hash_skill_primary(d.path()).unwrap().unwrap();
        let full = hash_skill_dir(d.path()).unwrap();
        assert_ne!(primary, full);
    }

    #[test]
    fn primary_hash_missing_skill_md_is_none() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("README"), "not a skill").unwrap();
        assert!(hash_skill_primary(d.path()).unwrap().is_none());
    }

    #[test]
    fn should_tier_returns_false_for_small_skills() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "tiny").unwrap();
        assert!(!should_tier(d.path()).unwrap());
    }

    #[test]
    fn should_tier_triggers_on_aggregate_bytes() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "x".repeat(20 * 1024)).unwrap();
        assert!(should_tier(d.path()).unwrap());
    }

    #[test]
    fn should_tier_triggers_on_file_count() {
        let d = tempdir().unwrap();
        for i in 0..6 {
            fs::write(d.path().join(format!("f{i}.md")), "x").unwrap();
        }
        assert!(should_tier(d.path()).unwrap());
    }

    #[test]
    fn hash_rejects_trees_over_file_count_limit() {
        let d = tempdir().unwrap();
        for i in 0..=MAX_TOTAL_FILES {
            fs::write(d.path().join(format!("f{i}")), "x").unwrap();
        }
        let err = hash_skill_dir(d.path()).expect_err("must reject oversize tree");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("files"));
    }

    #[test]
    fn hash_rejects_trees_over_byte_limit() {
        let d = tempdir().unwrap();
        // One oversized file is enough — the cap is on aggregate raw
        // size, not per-file, but a single file above the cap trips it
        // without needing 100 MB of writes.
        let huge = vec![0u8; (MAX_TOTAL_BYTES + 1) as usize];
        fs::write(d.path().join("payload.bin"), &huge).unwrap();
        let err = hash_skill_dir(d.path()).expect_err("must reject oversize tree");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("bytes"));
    }
}
