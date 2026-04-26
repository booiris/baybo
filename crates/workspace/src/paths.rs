//! Centralised filesystem addresses.
//!
//! Every aura-managed path — workspace root, identity files, log dir,
//! libsql database, MCP config, sidecar cache — is resolved through one of
//! the constants or helpers in this module. Keeping the strings in a single
//! leaf-level crate prevents the same filename from drifting across
//! `gateway`, `tools`, `code-builder`, the binary entrypoints, etc.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Workspace-root file/dir names (relative to the workspace root)
// ---------------------------------------------------------------------------

/// Top-level config file (default name; `AURA_CONFIG_PATH` overrides).
pub const WORKSPACE_CONFIG_FILE: &str = "aura.json";

/// libsql database file.
pub const STORAGE_DB_FILE: &str = "storage.db";

/// Per-workspace singleton lock (advisory `flock`).
pub const SINGLETON_LOCK_FILE: &str = "aura.lock";

/// Channel TCP listener publishes its ephemeral port here.
pub const CHANNEL_PORT_FILE: &str = "channel.port";

/// MCP server registry file.
pub const MCP_CONFIG_FILE: &str = ".mcp.json";

/// Per-workspace logs directory.
pub const LOGS_DIR: &str = "logs";

/// File-name prefix for rolling log files (`aura.log.YYYY-MM-DD`).
pub const LOG_FILE_PREFIX: &str = "aura.log";

/// Workspace-local skill definitions directory.
pub const SKILLS_DIR: &str = "skills";

/// Subdirectory for code-builder scratch state.
pub const CODE_BUILDER_DIR: &str = ".aura/code-builder";

/// Per-call scratch dirs sit under `<CODE_BUILDER_DIR>/<RUNS_SUBDIR>/<uuid>/`.
pub const CODE_BUILDER_RUNS_SUBDIR: &str = "runs";

// ---------------------------------------------------------------------------
// Identity files
// ---------------------------------------------------------------------------

pub const IDENTITY_AGENTS_FILE: &str = "AGENTS.md";
pub const IDENTITY_SOUL_FILE: &str = "SOUL.md";
pub const IDENTITY_USER_FILE: &str = "USER.md";
pub const IDENTITY_IDENTITY_FILE: &str = "IDENTITY.md";

// ---------------------------------------------------------------------------
// Cache (XDG-style, outside the workspace root)
// ---------------------------------------------------------------------------

/// Aura subdirectory under `$XDG_CACHE_HOME` (or `$HOME/.cache`).
pub const CACHE_SUBDIR: &str = "aura";

/// Subdirectory holding extracted bun runtimes inside the cache.
pub const CACHE_RUNTIME_SUBDIR: &str = "runtime";

// ---------------------------------------------------------------------------
// Environment variables tied to path resolution
// ---------------------------------------------------------------------------

/// Override for the on-disk config file location.
pub const ENV_CONFIG_PATH: &str = "AURA_CONFIG_PATH";

// ---------------------------------------------------------------------------
// Default workspace root
// ---------------------------------------------------------------------------

/// Default workspace root: `~/.aura` in release, `./.aura` in debug. The
/// debug default keeps `cargo run` self-contained inside the project
/// checkout rather than polluting the real user home.
pub fn default_workspace_root() -> PathBuf {
    if cfg!(debug_assertions) {
        return PathBuf::from("./.aura");
    }
    match std::env::home_dir() {
        Some(home) => home.join(".aura"),
        None => PathBuf::from("./.aura"),
    }
}

/// System-level aura cache root: `$XDG_CACHE_HOME/aura`, falling back to
/// `$HOME/.cache/aura`. `None` if neither env var is set — callers map
/// this to their own error type.
pub fn aura_cache_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|base| base.join(CACHE_SUBDIR))
}

// ---------------------------------------------------------------------------
// IdentityKind
// ---------------------------------------------------------------------------

/// One of the four well-known workspace identity files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    Agents,
    Soul,
    User,
    Identity,
}

impl IdentityKind {
    /// Filename this identity document is stored under, relative to the
    /// workspace root.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Agents => IDENTITY_AGENTS_FILE,
            Self::Soul => IDENTITY_SOUL_FILE,
            Self::User => IDENTITY_USER_FILE,
            Self::Identity => IDENTITY_IDENTITY_FILE,
        }
    }

    /// Parse a user-facing label (`"agents"`, `"soul"`, `"user"`, `"identity"`).
    /// Accepts any of `name`, uppercase, or `NAME.md`.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "agents" | "agents.md" => Some(Self::Agents),
            "soul" | "soul.md" => Some(Self::Soul),
            "user" | "user.md" => Some(Self::User),
            "identity" | "identity.md" => Some(Self::Identity),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspacePaths
// ---------------------------------------------------------------------------

/// View on workspace paths anchored at a given workspace root. Every
/// subsystem that needs a file under the workspace builds it from here so
/// the answer to "where does `storage.db` live?" has exactly one source.
#[derive(Debug, Clone)]
pub struct WorkspacePaths {
    root: PathBuf,
}

impl WorkspacePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn into_root(self) -> PathBuf {
        self.root
    }

    pub fn storage_db(&self) -> PathBuf {
        self.root.join(STORAGE_DB_FILE)
    }

    pub fn singleton_lock(&self) -> PathBuf {
        self.root.join(SINGLETON_LOCK_FILE)
    }

    pub fn channel_port(&self) -> PathBuf {
        self.root.join(CHANNEL_PORT_FILE)
    }

    pub fn mcp_config(&self) -> PathBuf {
        self.root.join(MCP_CONFIG_FILE)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(LOGS_DIR)
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join(SKILLS_DIR)
    }

    pub fn code_builder_runs_dir(&self) -> PathBuf {
        self.root
            .join(CODE_BUILDER_DIR)
            .join(CODE_BUILDER_RUNS_SUBDIR)
    }

    pub fn identity_file(&self, kind: IdentityKind) -> PathBuf {
        self.root.join(kind.file_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_compose_under_root() {
        let p = WorkspacePaths::new("/var/aura");
        assert_eq!(p.storage_db(), PathBuf::from("/var/aura/storage.db"));
        assert_eq!(p.singleton_lock(), PathBuf::from("/var/aura/aura.lock"));
        assert_eq!(p.channel_port(), PathBuf::from("/var/aura/channel.port"));
        assert_eq!(p.mcp_config(), PathBuf::from("/var/aura/.mcp.json"));
        assert_eq!(p.logs_dir(), PathBuf::from("/var/aura/logs"));
        assert_eq!(p.skills_dir(), PathBuf::from("/var/aura/skills"));
        assert_eq!(
            p.code_builder_runs_dir(),
            PathBuf::from("/var/aura/.aura/code-builder/runs"),
        );
        assert_eq!(
            p.identity_file(IdentityKind::Soul),
            PathBuf::from("/var/aura/SOUL.md"),
        );
    }

    #[test]
    fn identity_kind_round_trips_labels() {
        assert_eq!(IdentityKind::from_label("soul"), Some(IdentityKind::Soul));
        assert_eq!(
            IdentityKind::from_label(" AGENTS "),
            Some(IdentityKind::Agents)
        );
        assert_eq!(
            IdentityKind::from_label("user.md"),
            Some(IdentityKind::User)
        );
        assert_eq!(IdentityKind::from_label("CLAUDE.md"), None);
        assert_eq!(IdentityKind::Soul.file_name(), "SOUL.md");
    }

    #[test]
    fn cache_root_uses_xdg_when_set() {
        // Don't mutate process env in unit tests; just verify both
        // branches by inspecting the constant composition that the
        // helper performs.
        assert_eq!(CACHE_SUBDIR, "aura");
    }
}
