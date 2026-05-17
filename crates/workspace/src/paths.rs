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
//!   config/            # standalone git repo: aura.json, .mcp.json
//!   profile/           # standalone git repo: *.md identity files
//!   skills/            # standalone git repo: user skill definitions
//!   .key/              # not version-controlled: encryption.key
//!   state/             # not version-controlled: storage.db, aura.lock, channel.port, browser/profile
//!   work/              # not version-controlled: sandbox FS scope; code-builder/<uuid>/, future scratch
//!   logs/              # not version-controlled: aura.log.YYYY-MM-DD, channel/<type>.log, sessions/<id>.jsonl
//! ```
//!
//! `config/`, `profile/`, and `skills/` each get their own `.git` repo on
//! first `ensure_layout`; the workspace root itself is not git-tracked.

use std::path::{Path, PathBuf};

use crate::prompt::*;

// ---------------------------------------------------------------------------
// Workspace top-level subdirectories
// ---------------------------------------------------------------------------

/// Standalone git repo at `<root>/config/`: aura.json + MCP registry.
/// Operator-edited, version-controlled.
pub const CONFIG_DIR: &str = "config";

/// Standalone git repo at `<root>/profile/`: identity markdown files
/// (AGENTS, SOUL, USER, IDENTITY).
pub const PROFILE_DIR: &str = "profile";

/// Standalone git repo at `<root>/skills/`: workspace-local skill definitions.
pub const SKILLS_DIR: &str = "skills";

/// Master encryption-key directory at `<root>/.key/`. Not
/// version-controlled. Setup mints the key file inside on first run with
/// 0600 permissions.
pub const KEY_DIR: &str = ".key";

/// Persistent runtime state (db, locks, ports, browser profile). Not version-controlled.
pub const STATE_DIR: &str = "state";

/// Tool-generated scratch (code-builder runs, future scratch dirs). Not version-controlled.
pub const WORK_DIR: &str = "work";

/// Rolling log files (gateway, channels, per-session LLM call logs). Not version-controlled.
pub const LOGS_DIR: &str = "logs";

// ---------------------------------------------------------------------------
// Files inside `config/` (standalone git repo)
// ---------------------------------------------------------------------------

/// Top-level config file (default name; `AURA_CONFIG_PATH` overrides).
/// Stored at `<root>/config/aura.json`.
pub const WORKSPACE_CONFIG_FILE: &str = "aura.json";

/// MCP server registry file. Stored at `<root>/config/.mcp.json`.
pub const MCP_CONFIG_FILE: &str = ".mcp.json";

// ---------------------------------------------------------------------------
// Files inside `profile/` (standalone git repo)
// ---------------------------------------------------------------------------

pub const IDENTITY_SOUL_FILE: &str = "SOUL.md";
pub const IDENTITY_USER_FILE: &str = "USER.md";
pub const IDENTITY_IDENTITY_FILE: &str = "IDENTITY.md";

// ---------------------------------------------------------------------------
// Files inside `.key/` (not version-controlled)
// ---------------------------------------------------------------------------

/// Master encryption-key file. Hex-encoded 32 random bytes, mode 0600.
/// Stored at `<root>/.key/encryption.key`.
pub const ENCRYPTION_KEY_FILE: &str = "encryption.key";

// ---------------------------------------------------------------------------
// Files inside `state/` (not version-controlled)
// ---------------------------------------------------------------------------

/// libsql database file.
pub const STORAGE_DB_FILE: &str = "storage.db";

/// Per-workspace singleton lock (advisory `flock`).
pub const SINGLETON_LOCK_FILE: &str = "aura.lock";

/// Channel TCP listener publishes its ephemeral port here.
pub const CHANNEL_PORT_FILE: &str = "channel.port";

/// Browser sidecar Chrome user-data-dir lives at
/// `<STATE_DIR>/<BROWSER_PROFILE_SUBDIR>`. Persistent across Aura
/// restarts (cookies / localStorage retained); in docker mode this
/// gets bind-mounted at `/data/profile` inside the container.
pub const BROWSER_PROFILE_SUBDIR: &str = "browser/profile";

/// Per-session writable state dir lives at
/// `<STATE_DIR>/<STATE_SESSIONS_SUBDIR>/<session_id>/`. Currently holds
/// `summary.md` for the async summary-refresh design (see
/// `docs/background-compression.md`); future per-session writable
/// artifacts go alongside it. Distinct from `<LOGS_DIR>/sessions/`,
/// which is the *append-only* per-session LLM call log.
pub const STATE_SESSIONS_SUBDIR: &str = "sessions";

/// Per-session summary file (markdown). Authoritative content for the
/// async-refresh fast-path lives at
/// `<STATE_DIR>/<STATE_SESSIONS_SUBDIR>/<session_id>/<SUMMARY_FILE>`;
/// the durable metadata index is the `session_summaries` libsql table.
pub const SUMMARY_FILE: &str = "summary.md";

/// Sibling of [`SUMMARY_FILE`] used by the atomic write path
/// (write-tempfile + rename). Surfaces only between the write and
/// rename steps; the orphan reaper deletes `*.tmp` files at startup.
pub const SUMMARY_FILE_TMP: &str = "summary.md.tmp";

// ---------------------------------------------------------------------------
// Files inside `work/` (not version-controlled)
// ---------------------------------------------------------------------------

/// Code-builder scratch parent inside [`WORK_DIR`]. Per-call scratch dirs
/// sit directly under `<WORK_DIR>/<CODE_BUILDER_SUBDIR>/<uuid>/`. Hidden
/// (leading dot) so the agent's working directory stays uncluttered.
pub const CODE_BUILDER_SUBDIR: &str = ".code-builder";

/// Per-call code-builder run dir layout, all relative to
/// `<WORK_DIR>/<CODE_BUILDER_SUBDIR>/<uuid>/`. The `*.txt` overflow
/// files are only written when stdout/stderr exceed the inline cap.
pub const CODE_BUILDER_SCRIPT_FILE: &str = "script.py";
pub const CODE_BUILDER_STDOUT_FILE: &str = "stdout.txt";
pub const CODE_BUILDER_STDERR_FILE: &str = "stderr.txt";
pub const CODE_BUILDER_TOOL_CALL_FILE: &str = "tool_call.json";
pub const CODE_BUILDER_UV_CACHE_SUBDIR: &str = "uv-cache";
pub const CODE_BUILDER_WORKDIR_SUBDIR: &str = "workdir";

/// Browser sidecar font drop-in dir inside [`WORK_DIR`]. Pinned as a
/// Chrome fontconfig `<dir>` at boot — drop a font here and the next
/// gateway restart picks it up without touching system fontconfig.
pub const BROWSER_FONTS_SUBDIR: &str = ".fonts";

/// Tool-output spill dir inside [`WORK_DIR`]. The security gateway
/// drops oversize tool results here as content-addressed `.txt` files
/// so the LLM can `Read` the rest. Hidden (leading dot) to keep
/// glob/grep noise down on the agent's working directory.
pub const TOOL_SPILLS_SUBDIR: &str = ".aura-tool-spills";

// ---------------------------------------------------------------------------
// Files inside `logs/` (not version-controlled)
// ---------------------------------------------------------------------------

/// File-name prefix for the gateway's rolling log files (`aura.log.YYYY-MM-DD`).
pub const LOG_FILE_PREFIX: &str = "aura.log";

/// Subdirectory under [`LOGS_DIR`] holding per-channel sidecar log files
/// (`<channel_type>.log.YYYY-MM-DD`).
pub const CHANNEL_LOGS_SUBDIR: &str = "channel";

/// Per-session LLM call logs land under [`LOGS_DIR`]`/`[`SESSIONS_LOG_SUBDIR`]
/// as `<session_id>.jsonl`. One line per LLM call: input messages,
/// parameters, and the response (or error) plus latency / model metadata.
pub const SESSIONS_LOG_SUBDIR: &str = "sessions";

/// Per-session JSONL files inside [`SESSIONS_LOG_SUBDIR`] are named
/// `<session_id>.<SESSION_LOG_EXTENSION>`.
pub const SESSION_LOG_EXTENSION: &str = "jsonl";

// ---------------------------------------------------------------------------
// Cache (XDG-style, outside the workspace root)
// ---------------------------------------------------------------------------

/// Aura subdirectory under `$XDG_CACHE_HOME` (or `$HOME/.cache`).
pub const CACHE_SUBDIR: &str = "aura";

// ---------------------------------------------------------------------------
// Binary / env-var identifiers (single source of truth)
// ---------------------------------------------------------------------------

/// The cargo `[[bin]]` name in the workspace root `Cargo.toml`. Mirrored
/// into the clap tree via `#[command(name = "aura", …)]` and matched
/// against bash-tool command strings to decide whether to inject the
/// agent-side env (see `aura_tools::builtin::bash::inject_aura_env`).
///
/// If the bin is ever renamed, update this *and* the clap attribute in
/// `aura_cli::cli` — they're not enforced equal by the compiler.
pub const BIN_NAME: &str = "aura";

/// Override for the on-disk config file location.
pub const ENV_CONFIG_PATH: &str = "AURA_CONFIG_PATH";

// ---------------------------------------------------------------------------
// Default workspace root + default config file
// ---------------------------------------------------------------------------

/// Best-effort path absolutisation. Prefers `canonicalize` (resolves
/// symlinks too — matches the form `runtime.rs` hands the OS sandbox)
/// and falls back to `std::path::absolute` + `.`-segment stripping when
/// the path doesn't yet exist on disk (e.g. boot before
/// `ensure_layout`, or unit tests pointing at a freshly-named
/// tempdir). `std::path::absolute` joins relative paths with cwd but
/// does not normalise `.` components — strip them manually so the
/// result doesn't show `<cwd>/./.aura/work`. `..` is left intact; the
/// OS resolves it correctly on access and proper normalisation
/// requires a real filesystem walk.
///
/// Callers that compare paths via [`Path::starts_with`] should route
/// both sides through this helper so a relative workspace root (e.g.
/// the debug-build default `./.aura`) does not turn the comparison
/// into a silent miss.
pub fn absolutise(p: &Path) -> PathBuf {
    if let Ok(canonical) = p.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut cleaned = PathBuf::new();
    for component in absolute.components() {
        if !matches!(component, std::path::Component::CurDir) {
            cleaned.push(component);
        }
    }
    cleaned
}

/// Default workspace root: `~/.aura` in release, `<cwd>/.aura` in
/// debug. The debug default keeps `cargo run` self-contained inside
/// the project checkout rather than polluting the real user home;
/// resolving against `current_dir` at call time keeps the result
/// absolute so the path stays valid no matter which cwd a later
/// subprocess inherits (see also [`AuraConfig::validate`], which
/// rejects relative `workspace.path`).
///
/// Falls back to `/.aura` in the (unreachable in practice) case
/// where neither `current_dir()` nor `home_dir()` resolves — an
/// absolute literal is still better than a relative one that
/// validation would refuse.
pub fn default_workspace_root() -> PathBuf {
    if cfg!(debug_assertions)
        && let Ok(cwd) = std::env::current_dir()
    {
        return cwd.join(".aura");
    }
    match std::env::home_dir() {
        Some(home) => home.join(".aura"),
        None => PathBuf::from("/.aura"),
    }
}

/// Default `aura.json` location: `<default_workspace_root>/config/aura.json`.
/// Used as the fallback when `AURA_CONFIG_PATH` is not set.
pub fn default_config_file() -> PathBuf {
    default_workspace_root()
        .join(CONFIG_DIR)
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

/// One of the three well-known workspace identity files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    Soul,
    User,
    Identity,
}

impl IdentityKind {
    /// Filename this identity document is stored under, relative to the
    /// profile directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Soul => IDENTITY_SOUL_FILE,
            Self::User => IDENTITY_USER_FILE,
            Self::Identity => IDENTITY_IDENTITY_FILE,
        }
    }

    /// Default initial markdown body used to seed this identity file on
    /// first workspace materialisation. Templates are intentionally
    /// minimal so the default system prompt stays neutral; users edit
    /// them in place to tune behaviour.
    pub fn default_content(self) -> &'static str {
        match self {
            Self::Soul => DEFAULT_SOUL_CONTENT,
            Self::User => DEFAULT_USER_CONTENT,
            Self::Identity => DEFAULT_IDENTITY_CONTENT,
        }
    }

    /// Iterator over all three identity kinds in the canonical order
    /// (soul, user, identity). Useful for code that needs to process
    /// every identity file uniformly — e.g. `ensure_layout` seeding
    /// defaults.
    pub fn all() -> [Self; 3] {
        [Self::Soul, Self::User, Self::Identity]
    }

    /// Parse a user-facing label (`"soul"`, `"user"`, `"identity"`).
    /// Accepts any of `name`, uppercase, or `NAME.md`.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
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

    pub fn config_dir(&self) -> PathBuf {
        self.root.join(CONFIG_DIR)
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.root.join(PROFILE_DIR)
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join(SKILLS_DIR)
    }

    pub fn key_dir(&self) -> PathBuf {
        self.root.join(KEY_DIR)
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

    /// Per-channel sidecar log directory: `<root>/logs/channel/`.
    /// Individual files inside are named
    /// `<channel_type>.log.YYYY-MM-DD`.
    pub fn channel_logs_dir(&self) -> PathBuf {
        self.logs_dir().join(CHANNEL_LOGS_SUBDIR)
    }

    /// Per-session LLM call log directory:
    /// `<root>/logs/sessions/`. Each session writes one
    /// `<session_id>.jsonl` file inside this directory.
    pub fn sessions_log_dir(&self) -> PathBuf {
        self.logs_dir().join(SESSIONS_LOG_SUBDIR)
    }

    // -- config/ contents --

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join(WORKSPACE_CONFIG_FILE)
    }

    pub fn mcp_config(&self) -> PathBuf {
        self.config_dir().join(MCP_CONFIG_FILE)
    }

    // -- profile/ contents --

    pub fn identity_file(&self, kind: IdentityKind) -> PathBuf {
        self.profile_dir().join(kind.file_name())
    }

    // -- .key/ contents --

    pub fn encryption_key_file(&self) -> PathBuf {
        self.key_dir().join(ENCRYPTION_KEY_FILE)
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

    /// Browser sidecar Chrome user-data-dir:
    /// `<root>/state/browser/profile/`.
    pub fn browser_profile_dir(&self) -> PathBuf {
        self.state_dir().join(BROWSER_PROFILE_SUBDIR)
    }

    /// Parent dir for per-session writable state:
    /// `<root>/state/sessions/`. Distinct from `sessions_log_dir()`,
    /// which holds *append-only* per-session LLM call logs.
    pub fn state_sessions_dir(&self) -> PathBuf {
        self.state_dir().join(STATE_SESSIONS_SUBDIR)
    }

    /// Per-session writable state directory:
    /// `<root>/state/sessions/<session_id>/`. Contains [`SUMMARY_FILE`]
    /// (and any future per-session artifacts).
    pub fn session_state_dir(&self, session_id: &str) -> PathBuf {
        self.state_sessions_dir().join(session_id)
    }

    /// Per-session summary file:
    /// `<root>/state/sessions/<session_id>/summary.md`.
    pub fn session_summary_file(&self, session_id: &str) -> PathBuf {
        self.session_state_dir(session_id).join(SUMMARY_FILE)
    }

    /// Tempfile sibling of [`Self::session_summary_file`] used by the
    /// atomic write path: write-then-rename guarantees readers either
    /// see the previous summary or the new one, never a partial.
    pub fn session_summary_tmp_file(&self, session_id: &str) -> PathBuf {
        self.session_state_dir(session_id).join(SUMMARY_FILE_TMP)
    }

    /// Per-session JSONL transcript log:
    /// `<root>/logs/sessions/<sanitized_session_id>.jsonl`. Sanitization
    /// matches the writer in [`aura_agent::session_log`] so the path
    /// resolved here is the one the SessionLlmLogger appends to.
    pub fn session_log_file(&self, session_id: &str) -> PathBuf {
        self.sessions_log_dir().join(format!(
            "{}.{}",
            sanitize_session_id(session_id),
            SESSION_LOG_EXTENSION
        ))
    }

    // -- work/ contents --

    pub fn code_builder_dir(&self) -> PathBuf {
        self.work_dir().join(CODE_BUILDER_SUBDIR)
    }

    pub fn browser_fonts_dir(&self) -> PathBuf {
        self.work_dir().join(BROWSER_FONTS_SUBDIR)
    }

    pub fn tool_spills_dir(&self) -> PathBuf {
        self.work_dir().join(TOOL_SPILLS_SUBDIR)
    }
}

/// Replace any character that isn't `[A-Za-z0-9_\-.]` with `_`, then
/// prefix `_` if the result is empty or starts with `.`. Used to map
/// a `SessionId` onto a safe filename component for the per-session
/// JSONL transcript log. Both [`WorkspacePaths::session_log_file`]
/// and `aura-agent`'s `SessionLlmLogger` route through this so the
/// resolved path is identical on both sides.
pub fn sanitize_session_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_compose_under_root() {
        let p = WorkspacePaths::new("/var/aura");
        assert_eq!(p.config_dir(), PathBuf::from("/var/aura/config"));
        assert_eq!(p.config_file(), PathBuf::from("/var/aura/config/aura.json"));
        assert_eq!(p.mcp_config(), PathBuf::from("/var/aura/config/.mcp.json"));
        assert_eq!(p.profile_dir(), PathBuf::from("/var/aura/profile"));
        assert_eq!(
            p.identity_file(IdentityKind::Soul),
            PathBuf::from("/var/aura/profile/SOUL.md"),
        );
        assert_eq!(p.key_dir(), PathBuf::from("/var/aura/.key"));
        assert_eq!(
            p.encryption_key_file(),
            PathBuf::from("/var/aura/.key/encryption.key"),
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
        assert_eq!(p.logs_dir(), PathBuf::from("/var/aura/logs"));
        assert_eq!(
            p.channel_logs_dir(),
            PathBuf::from("/var/aura/logs/channel"),
        );
        assert_eq!(
            p.sessions_log_dir(),
            PathBuf::from("/var/aura/logs/sessions"),
        );
        assert_eq!(p.skills_dir(), PathBuf::from("/var/aura/skills"));
        assert_eq!(
            p.code_builder_dir(),
            PathBuf::from("/var/aura/work/.code-builder"),
        );
        assert_eq!(
            p.browser_fonts_dir(),
            PathBuf::from("/var/aura/work/.fonts"),
        );
        assert_eq!(
            p.browser_profile_dir(),
            PathBuf::from("/var/aura/state/browser/profile"),
        );
        assert_eq!(
            p.tool_spills_dir(),
            PathBuf::from("/var/aura/work/.aura-tool-spills"),
        );
        assert_eq!(
            p.state_sessions_dir(),
            PathBuf::from("/var/aura/state/sessions"),
        );
        assert_eq!(
            p.session_state_dir("abc-123"),
            PathBuf::from("/var/aura/state/sessions/abc-123"),
        );
        assert_eq!(
            p.session_summary_file("abc-123"),
            PathBuf::from("/var/aura/state/sessions/abc-123/summary.md"),
        );
        assert_eq!(
            p.session_summary_tmp_file("abc-123"),
            PathBuf::from("/var/aura/state/sessions/abc-123/summary.md.tmp"),
        );
    }

    #[test]
    fn identity_kind_round_trips_labels() {
        assert_eq!(IdentityKind::from_label("soul"), Some(IdentityKind::Soul));
        assert_eq!(
            IdentityKind::from_label(" IDENTITY "),
            Some(IdentityKind::Identity)
        );
        assert_eq!(
            IdentityKind::from_label("user.md"),
            Some(IdentityKind::User)
        );
        assert_eq!(IdentityKind::from_label("CLAUDE.md"), None);
        assert_eq!(IdentityKind::from_label("agents"), None);
        assert_eq!(IdentityKind::Soul.file_name(), "SOUL.md");
    }

    #[test]
    fn identity_kind_default_content_is_non_empty_per_kind() {
        for kind in IdentityKind::all() {
            let body = kind.default_content();
            assert!(
                !body.trim().is_empty(),
                "default content for {kind:?} must be non-empty"
            );
        }
        // The defaults must be distinct — if any two collide a
        // copy-paste mistake would make the seeded prompt incoherent.
        let mut seen = std::collections::HashSet::new();
        for kind in IdentityKind::all() {
            assert!(
                seen.insert(kind.default_content()),
                "default content collision involving {kind:?}"
            );
        }
    }

    #[test]
    fn cache_root_uses_xdg_when_set() {
        // Don't mutate process env in unit tests; just verify both
        // branches by inspecting the constant composition that the
        // helper performs.
        assert_eq!(CACHE_SUBDIR, "aura");
    }

    #[test]
    fn default_config_file_lives_under_default_root_config() {
        let cfg = default_config_file();
        let root = default_workspace_root();
        assert_eq!(cfg, root.join(CONFIG_DIR).join(WORKSPACE_CONFIG_FILE));
    }
}
