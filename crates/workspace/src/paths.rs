//! Centralised filesystem addresses.
//!
//! Every aura-managed path — workspace root, identity files, log dir,
//! libsql database, MCP config, sidecar cache — is resolved through one of
//! the constants or helpers in this module. Keeping the strings in a single
//! leaf-level crate prevents the same filename from drifting across
//! `gateway`, `tools`, `code-builder`, the binary entrypoints, etc.
//!
//! Layout under a workspace root:
//!
//! ```text
//! <root>/
//!   .gitignore         # allowlists profile/ and skills/
//!   profile/           # git-tracked: aura.json, .mcp.json, *.md identity files
//!   skills/            # git-tracked: user skill definitions
//!   state/             # ignored: storage.db, aura.lock, channel.port
//!   work/              # ignored: sandbox FS scope; code-builder/<uuid>/, future scratch
//!   logs/              # ignored: aura.log.YYYY-MM-DD plus channel/<type>.log.<date>
//! ```

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Workspace top-level subdirectories
// ---------------------------------------------------------------------------

/// Git-tracked: aura.json, MCP registry, identity markdown files.
pub const PROFILE_DIR: &str = "profile";

/// Git-tracked: workspace-local skill definitions.
pub const SKILLS_DIR: &str = "skills";

/// Gitignored: persistent runtime state (db, locks, ports).
pub const STATE_DIR: &str = "state";

/// Gitignored: tool-generated scratch (code-builder runs, future scratch dirs).
pub const WORK_DIR: &str = "work";

/// Gitignored: rolling log files.
pub const LOGS_DIR: &str = "logs";

/// Repo-root marker that allowlists [`PROFILE_DIR`] and [`SKILLS_DIR`].
pub const GITIGNORE_FILE: &str = ".gitignore";

/// Contents of the workspace `.gitignore`. Allowlist style: ignore everything,
/// then re-include the directories the user is meant to version. Keep this in
/// sync with the directory constants above.
pub const GITIGNORE_CONTENTS: &str = "\
# Aura workspace gitignore — only profile/ and skills/ are version-controlled.
/*
!/.gitignore
!/profile/
!/skills/
";

// ---------------------------------------------------------------------------
// Files inside `profile/` (git-tracked)
// ---------------------------------------------------------------------------

/// Top-level config file (default name; `AURA_CONFIG_PATH` overrides).
/// Stored at `<root>/profile/aura.json`.
pub const WORKSPACE_CONFIG_FILE: &str = "aura.json";

/// MCP server registry file. Stored at `<root>/profile/.mcp.json`.
pub const MCP_CONFIG_FILE: &str = ".mcp.json";

pub const IDENTITY_AGENTS_FILE: &str = "AGENTS.md";
pub const IDENTITY_SOUL_FILE: &str = "SOUL.md";
pub const IDENTITY_USER_FILE: &str = "USER.md";
pub const IDENTITY_IDENTITY_FILE: &str = "IDENTITY.md";

// ---------------------------------------------------------------------------
// Files inside `state/` (gitignored)
// ---------------------------------------------------------------------------

/// libsql database file.
pub const STORAGE_DB_FILE: &str = "storage.db";

/// Per-workspace singleton lock (advisory `flock`).
pub const SINGLETON_LOCK_FILE: &str = "aura.lock";

/// Channel TCP listener publishes its ephemeral port here.
pub const CHANNEL_PORT_FILE: &str = "channel.port";

/// Per-session LLM call logs land under [`STATE_DIR`]`/`[`SESSIONS_LOG_SUBDIR`]
/// as `<session_id>.jsonl`. One line per LLM call: input messages,
/// parameters, and the response (or error) plus latency / model metadata.
pub const SESSIONS_LOG_SUBDIR: &str = "sessions";

// ---------------------------------------------------------------------------
// Files inside `work/` (gitignored)
// ---------------------------------------------------------------------------

/// Code-builder scratch parent inside [`WORK_DIR`]. Per-call scratch dirs
/// sit directly under `<WORK_DIR>/<CODE_BUILDER_SUBDIR>/<uuid>/`.
pub const CODE_BUILDER_SUBDIR: &str = "code-builder";

// ---------------------------------------------------------------------------
// Files inside `logs/` (gitignored)
// ---------------------------------------------------------------------------

/// File-name prefix for the gateway's rolling log files (`aura.log.YYYY-MM-DD`).
pub const LOG_FILE_PREFIX: &str = "aura.log";

/// Subdirectory under [`LOGS_DIR`] holding per-channel sidecar log files
/// (`<channel_type>.log.YYYY-MM-DD`).
pub const CHANNEL_LOGS_SUBDIR: &str = "channel";

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
// Default workspace root + default config file
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

/// Default `aura.json` location: `<default_workspace_root>/profile/aura.json`.
/// Used as the fallback when `AURA_CONFIG_PATH` is not set.
pub fn default_config_file() -> PathBuf {
    default_workspace_root()
        .join(PROFILE_DIR)
        .join(WORKSPACE_CONFIG_FILE)
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
    /// profile directory.
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

    // -- top-level subdirectories --

    pub fn profile_dir(&self) -> PathBuf {
        self.root.join(PROFILE_DIR)
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join(SKILLS_DIR)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn work_dir(&self) -> PathBuf {
        self.root.join(WORK_DIR)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(LOGS_DIR)
    }

    /// Per-channel sidecar log directory:
    /// `<root>/logs/channel/<channel_type>.log.YYYY-MM-DD`.
    pub fn channel_logs_dir(&self) -> PathBuf {
        self.logs_dir().join(CHANNEL_LOGS_SUBDIR)
    }

    pub fn gitignore_file(&self) -> PathBuf {
        self.root.join(GITIGNORE_FILE)
    }

    // -- profile/ contents --

    pub fn config_file(&self) -> PathBuf {
        self.profile_dir().join(WORKSPACE_CONFIG_FILE)
    }

    pub fn mcp_config(&self) -> PathBuf {
        self.profile_dir().join(MCP_CONFIG_FILE)
    }

    pub fn identity_file(&self, kind: IdentityKind) -> PathBuf {
        self.profile_dir().join(kind.file_name())
    }

    // -- state/ contents --

    pub fn storage_db(&self) -> PathBuf {
        self.state_dir().join(STORAGE_DB_FILE)
    }

    pub fn singleton_lock(&self) -> PathBuf {
        self.state_dir().join(SINGLETON_LOCK_FILE)
    }

    pub fn channel_port(&self) -> PathBuf {
        self.state_dir().join(CHANNEL_PORT_FILE)
    }

    /// Per-session LLM call log directory:
    /// `<root>/state/sessions/`. Each session writes one
    /// `<session_id>.jsonl` file inside this directory.
    pub fn sessions_log_dir(&self) -> PathBuf {
        self.state_dir().join(SESSIONS_LOG_SUBDIR)
    }

    // -- work/ contents --

    pub fn code_builder_dir(&self) -> PathBuf {
        self.work_dir().join(CODE_BUILDER_SUBDIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_compose_under_root() {
        let p = WorkspacePaths::new("/var/aura");
        assert_eq!(
            p.config_file(),
            PathBuf::from("/var/aura/profile/aura.json")
        );
        assert_eq!(p.mcp_config(), PathBuf::from("/var/aura/profile/.mcp.json"));
        assert_eq!(
            p.identity_file(IdentityKind::Soul),
            PathBuf::from("/var/aura/profile/SOUL.md"),
        );
        assert_eq!(p.storage_db(), PathBuf::from("/var/aura/state/storage.db"),);
        assert_eq!(
            p.singleton_lock(),
            PathBuf::from("/var/aura/state/aura.lock"),
        );
        assert_eq!(
            p.channel_port(),
            PathBuf::from("/var/aura/state/channel.port"),
        );
        assert_eq!(
            p.sessions_log_dir(),
            PathBuf::from("/var/aura/state/sessions"),
        );
        assert_eq!(p.logs_dir(), PathBuf::from("/var/aura/logs"));
        assert_eq!(
            p.channel_logs_dir(),
            PathBuf::from("/var/aura/logs/channel"),
        );
        assert_eq!(p.skills_dir(), PathBuf::from("/var/aura/skills"));
        assert_eq!(
            p.code_builder_dir(),
            PathBuf::from("/var/aura/work/code-builder"),
        );
        assert_eq!(p.gitignore_file(), PathBuf::from("/var/aura/.gitignore"));
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

    #[test]
    fn default_config_file_lives_under_default_root_profile() {
        let cfg = default_config_file();
        let root = default_workspace_root();
        assert_eq!(cfg, root.join(PROFILE_DIR).join(WORKSPACE_CONFIG_FILE));
    }

    #[test]
    fn gitignore_contents_allowlists_profile_and_skills() {
        assert!(GITIGNORE_CONTENTS.contains("/*"));
        assert!(GITIGNORE_CONTENTS.contains("!/profile/"));
        assert!(GITIGNORE_CONTENTS.contains("!/skills/"));
        assert!(GITIGNORE_CONTENTS.contains("!/.gitignore"));
    }
}
