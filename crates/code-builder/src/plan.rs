use std::collections::HashSet;
use std::path::{Path, PathBuf};

use aura_sandbox::NetworkPolicy;

use crate::error::CodeBuilderError;
use crate::parse::RawPlan;

/// Hard ceilings applied to every CodeBuilder call. `EffectivePlan` is
/// always within these bounds regardless of what the LLM or caller asks
/// for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HardCaps {
    pub wall_clock_seconds: u64,
    pub memory_max_bytes: u64,
    pub pids_max: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub max_code_bytes: usize,
    pub max_readable_paths: usize,
    pub max_writable_paths: usize,
}

impl HardCaps {
    pub const fn defaults() -> Self {
        Self {
            wall_clock_seconds: 120,
            memory_max_bytes: 1024 * 1024 * 1024,
            pids_max: 64,
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 1024 * 1024,
            max_code_bytes: 64 * 1024,
            max_readable_paths: 16,
            max_writable_paths: 8,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CallerCaps {
    pub max_runtime_seconds: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub allow_network: bool,
    pub extra_readable_paths: Vec<PathBuf>,
    /// Caller-side allowlist of paths the LLM may declare in its
    /// `writable_paths`. Mirrors `extra_readable_paths`: the LLM cannot
    /// widen this list, and any out-of-scratch entry forces an
    /// approval prompt before the sandbox starts.
    pub extra_writable_paths: Vec<PathBuf>,
}

/// What the LLM declared a writable intent to be. The trailing slash
/// in the LLM's response tells us whether the script is going to write
/// **inside** a directory (`/foo/bar/`) or write **a file** at that
/// path (`/foo/bar`). The post-approval step uses this to keep the
/// sandbox bind as narrow as possible: dir intents bind the dir
/// itself (so siblings of the dir aren't exposed), file intents bind
/// the file's parent (just enough for `open()` to succeed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritableKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub(crate) struct WritableIntent {
    pub path: PathBuf,
    pub kind: WritableKind,
}

#[derive(Debug, Clone)]
pub(crate) struct EffectivePlan {
    pub code: String,
    pub network_policy: NetworkPolicy,
    /// LLM-supplied reason for needing the network. `Some` only when
    /// `network_policy == NetworkPolicy::All`.
    pub network_reason: Option<String>,
    pub readable_paths: Vec<PathBuf>,
    /// Out-of-scratch paths the script intends to write to, as the LLM
    /// declared them. These are the **display values** shown to the
    /// user in the approval prompt; the actual sandbox bind targets
    /// are computed post-approval (see
    /// `crate::tool::resolve_writable_bind_targets`) to keep the
    /// authorised scope minimal — dir intents bind the dir itself,
    /// file intents bind the parent dir, all `mkdir -p`'d after the
    /// user agrees.
    pub writable_paths: Vec<WritableIntent>,
    /// Canonical form of `caps.extra_writable_paths` for the same call.
    /// Held on the plan so the post-approval bind-target resolution
    /// can re-validate (TOCTOU defense) that the parent we created
    /// still falls under a caller-granted directory.
    pub canonical_caller_writes: Vec<PathBuf>,
    pub wall_clock_seconds: u64,
    pub memory_max_bytes: u64,
    pub pids_max: u64,
    pub rationale: String,
}

const DEFAULT_RUNTIME_SECONDS: u64 = 60;
const DEFAULT_MEMORY_MB: u64 = 512;

/// Validate a `RawPlan` and project it into an `EffectivePlan` with all
/// permissions clamped against `caps` and `hard`. The LLM cannot widen
/// the caller's caps — every field is an intersection.
pub(crate) fn project(
    raw: RawPlan,
    caps: &CallerCaps,
    hard: &HardCaps,
) -> Result<EffectivePlan, CodeBuilderError> {
    if raw.code.len() > hard.max_code_bytes {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "generated code exceeds {} bytes (got {})",
            hard.max_code_bytes,
            raw.code.len()
        )));
    }

    let llm_paths: Vec<PathBuf> = raw
        .readable_paths
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    for p in &llm_paths {
        if !p.is_absolute() {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "readable_paths entries must be absolute: {p:?}"
            )));
        }
    }
    let mut canonical_caller: Vec<PathBuf> = Vec::with_capacity(caps.extra_readable_paths.len());
    for p in &caps.extra_readable_paths {
        let canon = canonical_for_check(p)?;
        check_path_safe_to_mount(&canon)?;
        canonical_caller.push(canon);
    }

    let caller_set: HashSet<&PathBuf> = canonical_caller.iter().collect();
    let mut readable_paths: Vec<PathBuf> = Vec::new();
    for raw_p in &llm_paths {
        let canon = canonical_for_check(raw_p)?;
        if caller_set.contains(&canon) {
            readable_paths.push(canon);
            if readable_paths.len() >= hard.max_readable_paths {
                break;
            }
        }
    }

    // Caller writable allowlist: unlike the read side, callers commonly
    // grant a path that doesn't exist yet (the script will create the
    // file or directory). Walk up to the deepest existing ancestor,
    // canonicalize it (resolving any symlinks in the existing portion),
    // then re-attach the not-yet-created tail components. The result is
    // the same lexical path the caller asked for, with symlinks in the
    // existing portion resolved — `check_path_safe_to_mount` then runs
    // on that canonical form.
    let mut canonical_caller_writes: Vec<PathBuf> =
        Vec::with_capacity(caps.extra_writable_paths.len());
    for p in &caps.extra_writable_paths {
        let canon = canonical_for_writable_caller(p)?;
        check_path_safe_to_mount(&canon)?;
        canonical_caller_writes.push(canon);
    }

    let mut writable_paths: Vec<WritableIntent> = Vec::new();
    for raw_str in &raw.writable_paths {
        // Trailing slash is the LLM's signal that the path is a
        // directory it wants to write inside. Detect *before*
        // converting to `PathBuf` because `Path` operations are
        // component-based and ignore the slash. We strip the slash
        // for the path itself and remember the intent in
        // `WritableKind`.
        let kind = if raw_str.ends_with('/') {
            WritableKind::Dir
        } else {
            WritableKind::File
        };
        let trimmed = raw_str.trim_end_matches('/');
        let path = if trimmed.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(trimmed)
        };

        if !path.is_absolute() {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "writable_paths entries must be absolute: {raw_str:?}"
            )));
        }
        // Reject `..` / `.` components so the lexical containment
        // check below cannot be tricked by `/grant/../escape`. Caller
        // dirs are already canonicalised, so the LLM's path must be
        // clean for `Path::starts_with` to be a sound containment
        // test.
        if !is_lexically_clean(&path) {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "writable_paths entries must be normalised (no `..` or `.`): {raw_str:?}"
            )));
        }
        // Lexical containment: the LLM's declared path must equal
        // *one* caller-granted directory or sit underneath it. We do
        // **not** canonicalize the path — it may legitimately not
        // exist yet (the script will create the file/dir). The
        // post-approval step (`tool::resolve_writable_bind_targets`)
        // does a real canonicalize after `mkdir -p` and re-runs
        // containment to catch any symlink planted between approval
        // and bind.
        let inside_caller = canonical_caller_writes
            .iter()
            .any(|c| path == *c || path.starts_with(c));
        if !inside_caller {
            // Silent drop matches the read-side semantics: an LLM
            // path outside the caller's allowlist simply doesn't
            // get bound, so the script's `open()` will fail at
            // runtime if it really needs that path.
            continue;
        }
        if !writable_paths.iter().any(|i| i.path == path) {
            writable_paths.push(WritableIntent { path, kind });
            if writable_paths.len() >= hard.max_writable_paths {
                break;
            }
        }
    }

    let llm_runtime = raw
        .estimated_runtime_seconds
        .unwrap_or(DEFAULT_RUNTIME_SECONDS)
        .max(1);
    let caller_runtime = caps.max_runtime_seconds.unwrap_or(DEFAULT_RUNTIME_SECONDS);
    let wall_clock_seconds = llm_runtime.min(caller_runtime).min(hard.wall_clock_seconds);

    let llm_mem_bytes = raw
        .estimated_memory_mb
        .unwrap_or(DEFAULT_MEMORY_MB)
        .saturating_mul(1024 * 1024)
        .max(64 * 1024 * 1024);
    let caller_mem_bytes = caps
        .max_memory_mb
        .unwrap_or(DEFAULT_MEMORY_MB)
        .saturating_mul(1024 * 1024);
    let memory_max_bytes = llm_mem_bytes
        .min(caller_mem_bytes)
        .min(hard.memory_max_bytes);

    let (network_policy, network_reason) = if caps.allow_network && raw.network_required {
        let reason = raw
            .network_reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                CodeBuilderError::LlmPlanRejected(
                    "network_required must include a non-empty network_reason".into(),
                )
            })?;
        (NetworkPolicy::All, Some(reason))
    } else {
        (NetworkPolicy::None, None)
    };

    Ok(EffectivePlan {
        code: raw.code,
        network_policy,
        network_reason,
        readable_paths,
        writable_paths,
        canonical_caller_writes,
        wall_clock_seconds,
        memory_max_bytes,
        pids_max: hard.pids_max,
        rationale: raw.rationale,
    })
}

/// `true` if `p` is absolute and contains only root and "normal" path
/// components — i.e. no `..` or `.` to confuse a `starts_with` check.
fn is_lexically_clean(p: &Path) -> bool {
    use std::path::Component;
    p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// System / user-data roots that mount entire trees of credential-bearing
/// directories underneath them. `is_sensitive_path` is suffix-based and
/// will not flag these on its own (e.g. `/home/u` is not flagged but
/// mounting it exposes `/home/u/.ssh/id_rsa`).
const BROAD_ROOT_DENY: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib32", "/lib64", "/media", "/mnt",
    "/opt", "/proc", "/root", "/run", "/sbin", "/srv", "/sys", "/tmp", "/usr", "/var", "/Users",
    "/private",
];

/// Minimum path-component count for an extra_readable_paths entry. Anything
/// shallower than `/a/b/c` (3 components after the root) is treated as a
/// broad mount even if not in the explicit denylist.
const MIN_PATH_COMPONENTS: usize = 3;

/// Canonicalize a caller-side **writable** path that may not exist yet.
/// Walks up to the deepest existing ancestor, canonicalises that
/// (resolving symlinks), then re-attaches the not-yet-existing tail
/// components verbatim. Reject lexically dirty input (`..`/`.`) so
/// re-attaching the tail is sound.
fn canonical_for_writable_caller(p: &Path) -> Result<PathBuf, CodeBuilderError> {
    if !p.is_absolute() {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_writable_paths entries must be absolute: {p:?}"
        )));
    }
    if !is_lexically_clean(p) {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_writable_paths entries must be normalised (no `..` or `.`): {p:?}"
        )));
    }
    // Try fast path first — if the whole path exists, just canonicalize.
    if let Ok(canon) = std::fs::canonicalize(p) {
        return Ok(canon);
    }
    // Walk up. Collect tail components in reverse order while peeling.
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current: PathBuf = p.to_path_buf();
    while !current.exists() {
        let Some(name) = current.file_name().map(|n| n.to_owned()) else {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "extra_writable_paths entry has no existing ancestor: {p:?}"
            )));
        };
        suffix.push(name);
        if !current.pop() {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "extra_writable_paths entry has no existing ancestor: {p:?}"
            )));
        }
    }
    let canonical_ancestor = std::fs::canonicalize(&current).map_err(|e| {
        CodeBuilderError::LlmPlanRejected(format!(
            "cannot canonicalize ancestor {current:?} of {p:?}: {e}"
        ))
    })?;
    let mut result = canonical_ancestor;
    for name in suffix.iter().rev() {
        result.push(name);
    }
    Ok(result)
}

fn canonical_for_check(p: &Path) -> Result<PathBuf, CodeBuilderError> {
    if !p.is_absolute() {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "readable_paths entries must be absolute: {p:?}"
        )));
    }
    // Hard-fail on canonicalize errors. Falling back to the raw path
    // (a) lets symlinks like `/var/foo -> /etc/passwd` slip through the
    // suffix-based `is_sensitive_path` check, and (b) makes the LLM /
    // caller intersection mismatch when one side gives the canonical
    // form and the other gives a non-canonical one. If the path
    // doesn't exist at validation time, mounting it would fail in the
    // sandbox anyway — surface the problem now with a clear reason.
    std::fs::canonicalize(p)
        .map_err(|e| CodeBuilderError::LlmPlanRejected(format!("cannot canonicalize {p:?}: {e}")))
}

fn check_path_safe_to_mount(p: &Path) -> Result<(), CodeBuilderError> {
    let s = p.to_string_lossy();
    if BROAD_ROOT_DENY.iter().any(|d| s.as_ref() == *d) {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_readable_paths refuses broad system root: {p:?}"
        )));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && p == Path::new(&home)
    {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_readable_paths refuses caller's $HOME directly: {p:?}"
        )));
    }
    let components = p
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
        .count();
    if components < MIN_PATH_COMPONENTS {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_readable_paths entries must be at least {MIN_PATH_COMPONENTS} components deep: {p:?}"
        )));
    }
    if aura_security::is_sensitive_path(p) {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_readable_paths includes a sensitive path: {p:?}"
        )));
    }
    if let Some(parent) = p.parent() {
        let parent_str = parent.to_string_lossy();
        if BROAD_ROOT_DENY.iter().any(|d| parent_str.as_ref() == *d)
            && components < MIN_PATH_COMPONENTS + 1
        {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "extra_readable_paths entry's parent is a broad system root: {p:?}"
            )));
        }
    }
    if has_sensitive_descendant(p) {
        return Err(CodeBuilderError::LlmPlanRejected(format!(
            "extra_readable_paths exposes a sensitive descendant under: {p:?}"
        )));
    }
    Ok(())
}

/// Walk `root` up to a small bounded depth and check each entry against
/// `is_sensitive_path`. Catches the case where the *path itself* looks
/// fine but mounting it would expose `.ssh/`, `.aws/`, `.env`, etc. under
/// it. Bounded depth keeps the cost low for typical agent-issued paths.
fn has_sensitive_descendant(root: &Path) -> bool {
    use std::collections::VecDeque;
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    let max_depth = 3;
    let max_entries = 256;
    let mut seen = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > max_entries {
                return false; // bounded; trust the static checks beyond this.
            }
            let path = entry.path();
            if aura_security::is_sensitive_path(&path) {
                return true;
            }
            if depth < max_depth && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                queue.push_back((path, depth + 1));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_default() -> CallerCaps {
        CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        }
    }

    fn raw(code: &str) -> RawPlan {
        RawPlan {
            code: code.into(),
            network_required: false,
            network_reason: None,
            readable_paths: vec![],
            writable_paths: vec![],
            estimated_runtime_seconds: Some(5),
            estimated_memory_mb: Some(64),
            rationale: "test".into(),
        }
    }

    #[test]
    fn happy_path_compiles_plan() {
        let plan = project(raw("print(1+1)"), &caps_default(), &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::None);
        assert_eq!(plan.wall_clock_seconds, 5);
    }

    #[test]
    fn rejects_relative_readable_path() {
        let mut r = raw("print(1)");
        r.readable_paths = vec!["./data.csv".into()];
        let err = project(r, &caps_default(), &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn caller_caps_clamp_llm_runtime() {
        let mut r = raw("print(1)");
        r.estimated_runtime_seconds = Some(600);
        let caps = CallerCaps {
            max_runtime_seconds: Some(10),
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.wall_clock_seconds, 10);
    }

    #[test]
    fn hard_caps_clamp_caller() {
        let mut r = raw("print(1)");
        r.estimated_runtime_seconds = Some(9999);
        r.estimated_memory_mb = Some(99999);
        let caps = CallerCaps {
            max_runtime_seconds: Some(9999),
            max_memory_mb: Some(99999),
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.wall_clock_seconds, 120);
        assert_eq!(plan.memory_max_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn network_off_when_llm_says_no_even_if_caller_allows() {
        let mut r = raw("print(1)");
        r.network_required = false;
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::None);
    }

    #[test]
    fn network_off_when_caller_disallows_even_if_llm_says_yes() {
        let mut r = raw("print(1)");
        r.network_required = true;
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::None);
    }

    #[test]
    fn network_on_only_when_both_agree() {
        let mut r = raw("print(1)");
        r.network_required = true;
        r.network_reason = Some("fetch a remote api".into());
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::All);
    }

    #[test]
    fn llm_cannot_widen_readable_paths() {
        let mut r = raw("print(1)");
        r.readable_paths = vec!["/etc/passwd".into()];
        let plan = project(r, &caps_default(), &HardCaps::defaults()).unwrap();
        assert!(plan.readable_paths.is_empty());
    }

    #[test]
    fn caller_paths_only_pass_when_llm_lists_them_too() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a 3+ component canonical path inside a tempdir.
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("data");
        std::fs::create_dir_all(&allowed).unwrap();

        let mut r = raw("print(1)");
        r.readable_paths = vec![allowed.display().to_string()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![allowed.clone()],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.readable_paths, vec![allowed]);
    }

    #[test]
    fn oversized_code_rejected() {
        let mut r = raw("");
        r.code = "x".repeat(64 * 1024 + 1);
        let err = project(r, &caps_default(), &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn rejects_caller_sensitive_path() {
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![PathBuf::from("/home/u/.ssh/id_rsa")],
            extra_writable_paths: vec![],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn rejects_broad_root_mounts() {
        // Adversarial review #2: parents of sensitive directories must
        // be refused even when the parent itself isn't `is_sensitive_path`.
        for broad in [
            "/", "/home", "/Users", "/root", "/etc", "/var", "/usr", "/tmp", "/opt",
        ] {
            let caps = CallerCaps {
                max_runtime_seconds: None,
                max_memory_mb: None,
                allow_network: false,
                extra_readable_paths: vec![PathBuf::from(broad)],
                extra_writable_paths: vec![],
            };
            let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
            assert!(
                matches!(err, CodeBuilderError::LlmPlanRejected(_)),
                "broad root {broad} not rejected"
            );
        }
    }

    #[test]
    fn rejects_shallow_mounts_under_broad_root() {
        // `/home/user` is shallower than 3 components and a child of /home;
        // it would expose `.ssh/`, `.aws/`, etc. Must be refused.
        for shallow in ["/home/user", "/Users/alice", "/root/subdir"] {
            let caps = CallerCaps {
                max_runtime_seconds: None,
                max_memory_mb: None,
                allow_network: false,
                extra_readable_paths: vec![PathBuf::from(shallow)],
                extra_writable_paths: vec![],
            };
            let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
            assert!(
                matches!(err, CodeBuilderError::LlmPlanRejected(_)),
                "shallow path {shallow} not rejected"
            );
        }
    }

    #[test]
    fn rejects_caller_home() {
        let home = match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => return,
        };
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![PathBuf::from(&home)],
            extra_writable_paths: vec![],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn rejects_nonexistent_caller_path() {
        // Canonicalize must error rather than silently fall back to the
        // raw path. A typo'd caller path that happens to bypass the
        // depth/denylist check would otherwise be accepted, and a
        // symlink whose target is sensitive would slip past the
        // suffix-based `is_sensitive_path` check.
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![PathBuf::from("/data/this/path/definitely/does/not/exist")],
            extra_writable_paths: vec![],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn canonicalizes_symlinks_before_sensitivity_check() {
        // Symlink whose target lives under `.ssh/`. Without
        // canonicalization the suffix check on the *symlink* path
        // wouldn't see `/.ssh/` and would let it through.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("u").join(".ssh");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("id_rsa"), b"x").unwrap();
        let symlink_dir = tmp.path().join("decoy");
        std::os::unix::fs::symlink(&real, &symlink_dir).unwrap();

        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![symlink_dir],
            extra_writable_paths: vec![],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn rejects_dir_with_sensitive_descendant() {
        // Mount a tempdir that contains `.ssh/`. Even though the temp path
        // itself is fine, the descendant scan should refuse.
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("workspace").join("project");
        std::fs::create_dir_all(nested.join(".ssh")).unwrap();
        std::fs::write(nested.join(".ssh").join("id_rsa"), b"x").unwrap();

        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![nested.clone()],
            extra_writable_paths: vec![],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(_)),
            "nested .ssh under {nested:?} not detected"
        );
    }

    #[test]
    fn network_required_without_reason_is_rejected() {
        let mut r = raw("print(1)");
        r.network_required = true;
        r.network_reason = None;
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let err = project(r, &caps, &HardCaps::defaults()).unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(ref m) if m.contains("network_reason")),
            "got: {err:?}"
        );
    }

    #[test]
    fn network_required_with_blank_reason_is_rejected() {
        let mut r = raw("print(1)");
        r.network_required = true;
        r.network_reason = Some("   \t  ".into());
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let err = project(r, &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }

    #[test]
    fn network_reason_carried_into_effective_plan() {
        let mut r = raw("print(1)");
        r.network_required = true;
        r.network_reason = Some("  fetch json from api  ".into());
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::All);
        assert_eq!(plan.network_reason.as_deref(), Some("fetch json from api"));
    }

    #[test]
    fn network_reason_dropped_when_network_off() {
        let mut r = raw("print(1)");
        r.network_required = false;
        r.network_reason = Some("ignored".into());
        let plan = project(r, &caps_default(), &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::None);
        assert!(plan.network_reason.is_none());
    }

    #[test]
    fn writable_paths_must_be_absolute() {
        let mut r = raw("print(1)");
        r.writable_paths = vec!["./out".into()];
        let err = project(r, &caps_default(), &HardCaps::defaults()).unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(ref m) if m.contains("writable_paths")),
            "got: {err:?}"
        );
    }

    #[test]
    fn llm_cannot_widen_writable_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("a").join("b");
        std::fs::create_dir_all(&allowed).unwrap();

        let mut r = raw("print(1)");
        // LLM declares a path NOT under the caller's allowlist. The
        // walk-up logic resolves it (or its existing ancestors) but
        // none of them match a caller-granted dir, so the entry is
        // silently dropped — `writable_paths` ends up empty.
        r.writable_paths = vec!["/data/elsewhere".into()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert!(
            plan.writable_paths.is_empty(),
            "LLM path outside caller's allowlist must be dropped, got {:?}",
            plan.writable_paths
        );
    }

    #[test]
    fn caller_writable_passes_when_llm_lists_it() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("output");
        std::fs::create_dir_all(&allowed).unwrap();

        let mut r = raw("print(1)");
        r.writable_paths = vec![allowed.display().to_string()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.writable_paths.len(), 1);
        assert_eq!(plan.writable_paths[0].path, allowed);
        // No trailing slash on the raw input → File intent.
        assert_eq!(plan.writable_paths[0].kind, WritableKind::File);
    }

    #[test]
    fn writable_path_with_trailing_slash_is_classified_as_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("output");
        std::fs::create_dir_all(&allowed).unwrap();
        let sub = allowed.join("sub");

        let mut r = raw("print(1)");
        r.writable_paths = vec![format!("{}/", sub.display())];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.writable_paths.len(), 1);
        // Trailing slash stripped from the stored path; kind = Dir.
        assert_eq!(plan.writable_paths[0].path, sub);
        assert_eq!(plan.writable_paths[0].kind, WritableKind::Dir);
    }

    #[test]
    fn llm_can_declare_nonexistent_descendant_under_caller_dir() {
        // The LLM's intent is stored verbatim (the user approves the
        // specific declared path). The post-approval bind-target
        // resolution lives in tool::resolve_writable_bind_targets and
        // is tested separately.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("output");
        std::fs::create_dir_all(&allowed).unwrap();

        let intent = allowed.join("today.csv");
        let mut r = raw("print(1)");
        r.writable_paths = vec![intent.display().to_string()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.writable_paths.len(), 1);
        assert_eq!(
            plan.writable_paths[0].path, intent,
            "EffectivePlan keeps the LLM's specific declared path; bind target is computed post-approval"
        );
        assert_eq!(plan.writable_paths[0].kind, WritableKind::File);
        assert_eq!(plan.canonical_caller_writes, vec![allowed]);
    }

    #[test]
    fn llm_writable_path_outside_caller_set_is_silently_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("output");
        std::fs::create_dir_all(&allowed).unwrap();

        let mut r = raw("print(1)");
        // Lexically outside the caller's allowlist — silently dropped
        // at projection so the approval gate never sees it.
        r.writable_paths = vec!["/some/elsewhere/file.csv".into()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert!(plan.writable_paths.is_empty());
    }

    #[test]
    fn deep_nested_target_inside_caller_dir_keeps_full_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("project")
            .join("output");
        std::fs::create_dir_all(&allowed).unwrap();

        let intent = allowed.join("subdir").join("nested").join("file.csv");
        let mut r = raw("print(1)");
        r.writable_paths = vec![intent.display().to_string()];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed.clone()],
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        // The intent — including the not-yet-existing subdirs — is
        // preserved verbatim. The post-approval step will mkdir -p
        // the parent (`subdir/nested`) and bind it.
        assert_eq!(plan.writable_paths.len(), 1);
        assert_eq!(plan.writable_paths[0].path, intent);
    }

    #[test]
    fn writable_path_with_dotdot_is_rejected() {
        // Defense against `starts_with` being fooled by `/grant/../escape`.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("a").join("b");
        std::fs::create_dir_all(&allowed).unwrap();

        let mut r = raw("print(1)");
        let dirty = format!("{}/../../escape/file", allowed.display());
        r.writable_paths = vec![dirty];
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![allowed],
        };
        let err = project(r, &caps, &HardCaps::defaults()).unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(ref m) if m.contains("normalised")),
            "got: {err:?}"
        );
    }

    #[test]
    fn writable_path_to_sensitive_dir_is_hard_rejected() {
        // Symmetric with `rejects_caller_sensitive_path` for reads.
        // A caller-side `extra_writable_paths` entry pointing into
        // `.ssh/`, `.aws/`, etc. is refused at projection time so
        // the approval gate never runs for credential-bearing dirs.
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![PathBuf::from("/home/u/.ssh")],
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(matches!(err, CodeBuilderError::LlmPlanRejected(_)));
    }
}
