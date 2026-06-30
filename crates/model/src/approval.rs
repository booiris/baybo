//! Approval-related value types shared between `baybo-tools` (which produces
//! them at runtime) and `baybo-context` / `baybo-model` (which persists
//! "approve always" grants on the session).
//!
//! Matching semantics (see each enum for details):
//! - File paths use prefix matching — approving a directory covers the subtree.
//! - Hosts use exact or wildcard (self + any subdomain) match.
//! - Shell commands use exact string equality.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Decision returned by an approval gate.
///
/// Lives in `baybo-model` so trace `SpanEvent`s can record approval
/// outcomes without `baybo-trace` having to depend on `baybo-tools`.
/// `baybo-tools` re-exports this type so existing call sites keep
/// working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum ApprovalDecision {
    /// Allow this call only.
    Approve,
    /// Allow this call and remember every resource it touches for the rest of
    /// the session (persisted via `SessionState::approved_resources`).
    ApproveAlways,
    /// Reject the call. The executor surfaces this as `ToolError::Denied`.
    Deny,
}

/// Concrete resource a single tool call touches, derived from its parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum ResourceAccess {
    ReadFile {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        path: PathBuf,
    },
    WriteFile {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        path: PathBuf,
    },
    Http {
        host: String,
    },
    ExecCommand {
        command: String,
    },
    /// A skill-style declaration that the call wants the listed
    /// environment variables read on the user's behalf. Match is exact
    /// set equality after sort+dedup; approving `[FOO, BAR]` does NOT
    /// cover a later request for `[FOO]`. Env approvals are not
    /// auto-cached by `to_approved` — they are sensitive enough to
    /// justify per-session re-prompting.
    Env {
        vars: Vec<String>,
    },
}

impl ResourceAccess {
    /// Convert into the "approve always" cache entry that covers exactly this
    /// access. Hosts are lowercased on the way in and stored as
    /// `HostPattern::Exact`.
    pub fn to_approved(&self) -> ApprovedResource {
        match self {
            ResourceAccess::ReadFile { path } => ApprovedResource::ReadFile { path: path.clone() },
            ResourceAccess::WriteFile { path } => {
                ApprovedResource::WriteFile { path: path.clone() }
            }
            ResourceAccess::Http { host } => ApprovedResource::Http {
                host: HostPattern::exact(host),
            },
            ResourceAccess::ExecCommand { command } => ApprovedResource::ExecCommand {
                command: command.clone(),
            },
            ResourceAccess::Env { vars } => ApprovedResource::Env {
                vars: normalize_vars(vars),
            },
        }
    }
}

fn normalize_vars(vars: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vars.to_vec();
    out.sort();
    out.dedup();
    out
}

/// Persistent "approve always" entry stored on `SessionState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovedResource {
    ReadFile { path: PathBuf },
    WriteFile { path: PathBuf },
    Http { host: HostPattern },
    ExecCommand { command: String },
    Env { vars: Vec<String> },
}

impl ApprovedResource {
    /// True iff this approval entry covers the given access.
    pub fn covers(&self, access: &ResourceAccess) -> bool {
        match (self, access) {
            (
                ApprovedResource::ReadFile { path: approved },
                ResourceAccess::ReadFile { path: requested },
            ) => path_covers(approved, requested),
            (
                ApprovedResource::WriteFile { path: approved },
                ResourceAccess::WriteFile { path: requested },
            ) => path_covers(approved, requested),
            (
                ApprovedResource::Http { host: pattern },
                ResourceAccess::Http { host: requested },
            ) => pattern.matches(requested),
            (
                ApprovedResource::ExecCommand { command: approved },
                ResourceAccess::ExecCommand { command: requested },
            ) => approved == requested,
            (ApprovedResource::Env { vars: approved }, ResourceAccess::Env { vars: requested }) => {
                approved.as_slice() == normalize_vars(requested).as_slice()
            }
            _ => false,
        }
    }
}

/// Component-aware prefix match: `/tmp/a` covers `/tmp/a/b` but NOT `/tmp/ab`.
fn path_covers(approved: &Path, requested: &Path) -> bool {
    requested.starts_with(approved)
}

/// Host pattern on `ApprovedResource::Http`.
///
/// Construct via [`HostPattern::exact`] / [`HostPattern::wildcard`]
/// rather than the variants directly: those constructors lowercase
/// the host so two equivalent-but-different-case entries
/// (`api.GitHub.com` vs `api.github.com`) don't end up as separate
/// `ApprovedResource` rows under PartialEq. `matches()` is already
/// case-insensitive on both sides, so the only regression a
/// non-normalized variant produces is dedup drift in stored
/// approvals — but that's enough to make repeat approvals show up
/// as new prompts to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostPattern {
    /// Exact host, case-insensitive at match time.
    Exact(String),
    /// The host itself and every subdomain under it.
    /// `Wildcard("foo.com")` matches `foo.com` and `api.foo.com` but not
    /// `barfoo.com`.
    Wildcard(String),
}

impl HostPattern {
    /// Construct an `Exact` host pattern with the host lowercased.
    pub fn exact(host: impl AsRef<str>) -> Self {
        Self::Exact(host.as_ref().to_ascii_lowercase())
    }

    /// Construct a `Wildcard` host pattern with the host lowercased.
    pub fn wildcard(host: impl AsRef<str>) -> Self {
        Self::Wildcard(host.as_ref().to_ascii_lowercase())
    }

    pub fn matches(&self, host: &str) -> bool {
        match self {
            HostPattern::Exact(h) => h.eq_ignore_ascii_case(host),
            HostPattern::Wildcard(h) => {
                if h.eq_ignore_ascii_case(host) {
                    return true;
                }
                host.len() > h.len() + 1
                    && host.as_bytes()[host.len() - h.len() - 1] == b'.'
                    && host[host.len() - h.len()..].eq_ignore_ascii_case(h)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefix_covers_subtree_but_not_siblings() {
        let approved = ApprovedResource::ReadFile {
            path: PathBuf::from("/tmp/a"),
        };
        assert!(approved.covers(&ResourceAccess::ReadFile {
            path: PathBuf::from("/tmp/a/b.txt"),
        }));
        assert!(approved.covers(&ResourceAccess::ReadFile {
            path: PathBuf::from("/tmp/a"),
        }));
        assert!(!approved.covers(&ResourceAccess::ReadFile {
            path: PathBuf::from("/tmp/ab"),
        }));
    }

    #[test]
    fn read_and_write_do_not_cross() {
        let r = ApprovedResource::ReadFile {
            path: PathBuf::from("/tmp"),
        };
        assert!(!r.covers(&ResourceAccess::WriteFile {
            path: PathBuf::from("/tmp/x"),
        }));
    }

    #[test]
    fn host_exact_is_case_insensitive() {
        let p = HostPattern::Exact("GitHub.com".into());
        assert!(p.matches("github.com"));
        assert!(p.matches("GITHUB.COM"));
        assert!(!p.matches("api.github.com"));
    }

    #[test]
    fn host_wildcard_matches_self_and_subdomains_only() {
        let p = HostPattern::Wildcard("foo.com".into());
        assert!(p.matches("foo.com"));
        assert!(p.matches("api.foo.com"));
        assert!(p.matches("a.b.foo.com"));
        assert!(!p.matches("barfoo.com"));
        assert!(!p.matches("foo.com.evil"));
    }

    #[test]
    fn host_pattern_constructors_lowercase() {
        // Pin: equivalent-but-different-case patterns dedup under
        // PartialEq when both go through the constructor.
        assert_eq!(
            HostPattern::exact("api.GitHub.com"),
            HostPattern::exact("api.github.com"),
        );
        assert_eq!(
            HostPattern::wildcard("Foo.Com"),
            HostPattern::wildcard("foo.com"),
        );
        // Variant-direct construction still allows the drift the
        // constructors guard against — that's the whole reason
        // ResourceAccess::to_approved goes through the constructor.
        assert_ne!(
            HostPattern::Wildcard("Foo.Com".into()),
            HostPattern::Wildcard("foo.com".into()),
        );
    }

    #[test]
    fn env_vars_match_after_normalization() {
        // Approving [FOO, BAR] covers a request for [BAR, FOO] after sort+dedup.
        let approved = ResourceAccess::Env {
            vars: vec!["FOO".into(), "BAR".into()],
        }
        .to_approved();
        assert!(approved.covers(&ResourceAccess::Env {
            vars: vec!["BAR".into(), "FOO".into()],
        }));
        // Subset is NOT covered — env grants are exact set equality.
        assert!(!approved.covers(&ResourceAccess::Env {
            vars: vec!["FOO".into()],
        }));
        // Superset is NOT covered.
        assert!(!approved.covers(&ResourceAccess::Env {
            vars: vec!["FOO".into(), "BAR".into(), "BAZ".into()],
        }));
        // Duplicates in the request normalize away.
        assert!(approved.covers(&ResourceAccess::Env {
            vars: vec!["FOO".into(), "FOO".into(), "BAR".into()],
        }));
    }

    #[test]
    fn exec_command_exact_match() {
        let a = ApprovedResource::ExecCommand {
            command: "ls -la".into(),
        };
        assert!(a.covers(&ResourceAccess::ExecCommand {
            command: "ls -la".into(),
        }));
        assert!(!a.covers(&ResourceAccess::ExecCommand {
            command: "ls -la /tmp".into(),
        }));
    }
}
