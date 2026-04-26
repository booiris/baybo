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
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CallerCaps {
    pub max_runtime_seconds: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub allow_network: bool,
    pub extra_readable_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct EffectivePlan {
    pub code: String,
    pub network_policy: NetworkPolicy,
    pub readable_paths: Vec<PathBuf>,
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

    let network_policy = if caps.allow_network && raw.network_required {
        NetworkPolicy::All
    } else {
        NetworkPolicy::None
    };

    Ok(EffectivePlan {
        code: raw.code,
        network_policy,
        readable_paths,
        wall_clock_seconds,
        memory_max_bytes,
        pids_max: hard.pids_max,
        rationale: raw.rationale,
    })
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
        }
    }

    fn raw(code: &str) -> RawPlan {
        RawPlan {
            code: code.into(),
            network_required: false,
            readable_paths: vec![],
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
        };
        let plan = project(r, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.network_policy, NetworkPolicy::None);
    }

    #[test]
    fn network_on_only_when_both_agree() {
        let mut r = raw("print(1)");
        r.network_required = true;
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: true,
            extra_readable_paths: vec![],
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
        };
        let err = project(raw("print(1)"), &caps, &HardCaps::defaults()).unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(_)),
            "nested .ssh under {nested:?} not detected"
        );
    }
}
