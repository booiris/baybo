//! Approval-related value types shared between `aura-tools` (which produces
//! them at runtime) and `aura-session` / `aura-model` (which persists
//! "approve always" grants on the session).
//!
//! Matching semantics (see each enum for details):
//! - File paths use prefix matching — approving a directory covers the subtree.
//! - Hosts use exact or wildcard (self + any subdomain) match.
//! - Shell commands use exact string equality.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Concrete resource a single tool call touches, derived from its parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
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
                host: HostPattern::Exact(host.to_ascii_lowercase()),
            },
            ResourceAccess::ExecCommand { command } => ApprovedResource::ExecCommand {
                command: command.clone(),
            },
        }
    }
}

/// Persistent "approve always" entry stored on `SessionState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovedResource {
    ReadFile { path: PathBuf },
    WriteFile { path: PathBuf },
    Http { host: HostPattern },
    ExecCommand { command: String },
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
            _ => false,
        }
    }
}

/// Component-aware prefix match: `/tmp/a` covers `/tmp/a/b` but NOT `/tmp/ab`.
fn path_covers(approved: &Path, requested: &Path) -> bool {
    requested.starts_with(approved)
}

/// Host pattern on `ApprovedResource::Http`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostPattern {
    /// Exact host, case-insensitive.
    Exact(String),
    /// The host itself and every subdomain under it.
    /// `Wildcard("foo.com")` matches `foo.com` and `api.foo.com` but not
    /// `barfoo.com`.
    Wildcard(String),
}

impl HostPattern {
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
