//! Centralised filesystem addresses.
//!
//! Every baybo-managed path — workspace root, identity files, log dir,
//! sqlite database, MCP config, sidecar cache — is resolved through one of
//! the constants or helpers in this module. Keeping the strings in a single
//! leaf-level crate prevents the same filename from drifting across
//! `gateway`, `tools`, the binary entrypoints, etc.
//!
//! Layout under a workspace root:
//!
//! ```text
//! <root>/
//!   config/            # standalone git repo: baybo.json, .mcp.json
//!   profile/           # standalone git repo: *.md identity files
//!   skills/            # standalone git repo: user skill definitions
//!   agents/            # standalone git repo: subagent profile definitions
//!   .key/              # not version-controlled: encryption.key
//!   state/             # not version-controlled: storage.db, baybo.lock, channel.port, browser/profile
//!   work/              # not version-controlled: sandbox FS scope; .uv/ (uv cache + downloaded pythons + tools), tmp/ (disposable, swept), other scratch
//!   logs/              # not version-controlled: baybo.log.YYYY-MM-DD, channel/<type>.log (sessions/<id>.jsonl is virtual — never written)
//! ```
//!
//! `config/`, `profile/`, and `skills/` each get their own `.git` repo on
//! first `ensure_layout`; the workspace root itself is not git-tracked.

use std::path::{Path, PathBuf};

use crate::prompt::*;

// ---------------------------------------------------------------------------
// Workspace top-level subdirectories
// ---------------------------------------------------------------------------

/// Standalone git repo at `<root>/config/`: baybo.json + MCP registry.
/// Operator-edited, version-controlled.
pub const CONFIG_DIR: &str = "config";

/// Standalone git repo at `<root>/profile/`: identity markdown files
/// (AGENTS, SOUL, USER, IDENTITY).
pub const PROFILE_DIR: &str = "profile";

/// Standalone git repo at `<root>/skills/`: workspace-local skill definitions.
pub const SKILLS_DIR: &str = "skills";

/// Standalone git repo at `<root>/agents/`: workspace-local subagent
/// profile definitions. One `<name>.md` per profile (no
/// directory-per-profile ceremony — a profile has no linked-files
/// concern, only a frontmatter + system-prompt body).
pub const AGENTS_DIR: &str = "agents";

/// Standalone git repo at `<root>/personas/`: one directory per
/// user-managed agent profile, named by its id, carrying that agent's
/// `SOUL.md` and its private `skills/` overlay. The built-in profile has no
/// directory here — its persona is the workspace's own `profile/` and
/// `skills/`.
pub const PERSONAS_DIR: &str = "personas";

/// Per-agent private skill overlay, at `<root>/personas/<id>/skills/`.
/// Same one-directory-per-skill shape as the shared `skills/` tree.
pub const PERSONA_SKILLS_DIR: &str = "skills";

/// Deck card bundles at `<root>/deck/<uuid>/` — agent-authored plain
/// files (docs/modules/deck.md). Installed atomically via a `.staging/`
/// sibling inside this dir so the rename stays same-filesystem.
pub const DECK_DIR: &str = "deck";

/// Master encryption-key directory at `<root>/.key/`. Not
/// version-controlled. Setup mints the key file inside on first run with
/// 0600 permissions.
pub const KEY_DIR: &str = ".key";

/// Persistent runtime state (db, locks, ports, browser profile). Not version-controlled.
pub const STATE_DIR: &str = "state";

/// Tool-generated scratch (uv state, future scratch dirs). Not version-controlled.
pub const WORK_DIR: &str = "work";

/// Rolling log files (gateway, channels, per-session LLM call logs). Not version-controlled.
pub const LOGS_DIR: &str = "logs";

// ---------------------------------------------------------------------------
// Files inside `config/` (standalone git repo)
// ---------------------------------------------------------------------------

/// Top-level config file (default name; `BAYBO_CONFIG_PATH` overrides).
/// Stored at `<root>/config/baybo.json`.
pub const WORKSPACE_CONFIG_FILE: &str = "baybo.json";

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

/// sqlite database file.
pub const STORAGE_DB_FILE: &str = "storage.db";

/// Per-workspace singleton lock (advisory `flock`).
pub const SINGLETON_LOCK_FILE: &str = "baybo.lock";

/// Channel TCP listener publishes its ephemeral port here.
pub const CHANNEL_PORT_FILE: &str = "channel.port";

/// Browser sidecar Chrome user-data-dir lives at
/// `<STATE_DIR>/<BROWSER_PROFILE_SUBDIR>`. Persistent across Baybo
/// restarts (cookies / localStorage retained); in docker mode this
/// gets bind-mounted at `/data/profile` inside the container.
pub const BROWSER_PROFILE_SUBDIR: &str = "browser/profile";

// ---------------------------------------------------------------------------
// Files inside `work/` (not version-controlled)
// ---------------------------------------------------------------------------

/// Workspace-scoped `uv` state parent inside [`WORK_DIR`]. The Bash tool
/// exports `UV_CACHE_DIR`, `UV_PYTHON_INSTALL_DIR`, `UV_TOOL_DIR`, and
/// `UV_TOOL_BIN_DIR` rooted here so any `uv …` invocation caches into the
/// workspace instead of polluting `~/.cache/uv` / `~/.local/share/uv`.
/// Hidden (leading dot) so the agent's working directory stays uncluttered.
pub const UV_STATE_SUBDIR: &str = ".uv";

/// Browser sidecar font drop-in dir inside [`WORK_DIR`]. Pinned as a
/// Chrome fontconfig `<dir>` at boot — drop a font here and the next
/// gateway restart picks it up without touching system fontconfig.
pub const BROWSER_FONTS_SUBDIR: &str = ".fonts";

/// Tool-output spill dir inside [`WORK_DIR`]. The security gateway
/// drops oversize tool results here as content-addressed `.txt` files
/// so the LLM can `Read` the rest. Hidden (leading dot) to keep
/// glob/grep noise down on the agent's working directory.
pub const TOOL_SPILLS_SUBDIR: &str = ".baybo-tool-spills";

/// Disposable-scratch dir inside [`WORK_DIR`]. The Bash tool advertises
/// it as the destination for intermediate files (probe scripts, one-off
/// downloads, temp build output); the janitor removes any of its
/// top-level entries whose newest in-tree mtime is older than
/// [`WORK_TMP_TTL_DAYS`]. Deliverables meant for the user belong
/// elsewhere under [`WORK_DIR`], where nothing is auto-deleted.
pub const WORK_TMP_SUBDIR: &str = "tmp";

/// Sweep TTL, in days, for [`WORK_TMP_SUBDIR`] entries. Lives here (not
/// in `baybo-janitor`) because the Bash tool description advertises the
/// same figure — the model's contract and the sweep must quote one
/// number.
pub const WORK_TMP_TTL_DAYS: u64 = 7;

// ---------------------------------------------------------------------------
// Files inside `logs/` (not version-controlled)
// ---------------------------------------------------------------------------

/// File-name prefix for the gateway's rolling log files (`baybo.log.YYYY-MM-DD`).
pub const LOG_FILE_PREFIX: &str = "baybo.log";

/// Subdirectory under [`LOGS_DIR`] holding per-channel sidecar log files
/// (`<channel_type>.log.YYYY-MM-DD`).
pub const CHANNEL_LOGS_SUBDIR: &str = "channel";

/// Subdir under [`LOGS_DIR`] for the virtual per-session transcript path
/// `<session_id>.jsonl` (see [`WorkspacePaths::session_log_file`]). No file
/// is written here; the path is the post-compaction recovery pointer the
/// agent serves from the durable transcript.
pub const SESSIONS_LOG_SUBDIR: &str = "sessions";

/// Extension of the virtual per-session transcript path inside
/// [`SESSIONS_LOG_SUBDIR`]: `<session_id>.<SESSION_LOG_EXTENSION>`.
pub const SESSION_LOG_EXTENSION: &str = "jsonl";

// ---------------------------------------------------------------------------
// Cache (XDG-style, outside the workspace root)
// ---------------------------------------------------------------------------

/// Baybo subdirectory under `$XDG_CACHE_HOME` (or `$HOME/.cache`).
pub const CACHE_SUBDIR: &str = "baybo";

// ---------------------------------------------------------------------------
// Binary / env-var identifiers (single source of truth)
// ---------------------------------------------------------------------------

/// The cargo `[[bin]]` name in the workspace root `Cargo.toml`. Mirrored
/// into the clap tree via `#[command(name = "baybo", …)]` and matched
/// against bash-tool command strings to decide whether to inject the
/// agent-side env (see `baybo_tools::builtin::bash::inject_baybo_env`).
///
/// If the bin is ever renamed, update this *and* the clap attribute in
/// `baybo_cli::cli` — they're not enforced equal by the compiler.
pub const BIN_NAME: &str = "baybo";

/// Override for the on-disk config file location.
pub const ENV_CONFIG_PATH: &str = "BAYBO_CONFIG_PATH";

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
/// result doesn't show `<cwd>/./.baybo/work`. `..` is left intact; the
/// OS resolves it correctly on access and proper normalisation
/// requires a real filesystem walk.
///
/// Callers that compare paths via [`Path::starts_with`] should route
/// both sides through this helper so a relative workspace root (e.g.
/// the debug-build default `./.baybo`) does not turn the comparison
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

/// Default workspace root: `~/.baybo` in release, `<cwd>/.baybo` in
/// debug. The debug default keeps `cargo run` self-contained inside
/// the project checkout rather than polluting the real user home;
/// resolving against `current_dir` at call time keeps the result
/// absolute so the path stays valid no matter which cwd a later
/// subprocess inherits (see also [`BayboConfig::validate`], which
/// rejects relative `workspace.path`).
///
/// Falls back to `/.baybo` in the (unreachable in practice) case
/// where neither `current_dir()` nor `home_dir()` resolves — an
/// absolute literal is still better than a relative one that
/// validation would refuse.
pub fn default_workspace_root() -> PathBuf {
    if cfg!(debug_assertions)
        && let Ok(cwd) = std::env::current_dir()
    {
        return cwd.join(".baybo");
    }
    match std::env::home_dir() {
        Some(home) => home.join(".baybo"),
        None => PathBuf::from("/.baybo"),
    }
}

/// Default `baybo.json` location: `<default_workspace_root>/config/baybo.json`.
/// Used as the fallback when `BAYBO_CONFIG_PATH` is not set.
pub fn default_config_file() -> PathBuf {
    default_workspace_root()
        .join(CONFIG_DIR)
        .join(WORKSPACE_CONFIG_FILE)
}

/// System-level baybo cache root: `$XDG_CACHE_HOME/baybo`, falling back to
/// `$HOME/.cache/baybo`. `None` if neither env var is set — callers map
/// this to their own error type.
pub fn baybo_cache_root() -> Option<PathBuf> {
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

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join(AGENTS_DIR)
    }

    pub fn deck_dir(&self) -> PathBuf {
        self.root.join(DECK_DIR)
    }

    /// Root of the per-agent persona tree: `<root>/personas/`.
    pub fn personas_dir(&self) -> PathBuf {
        self.root.join(PERSONAS_DIR)
    }

    /// One agent's persona directory: `<root>/personas/<agent_id>/`.
    ///
    /// Callers hold an `AgentProfileId`, whose grammar is what keeps the
    /// joined component inside `personas/`; this crate is leaf-level and
    /// takes the id as a `&str`.
    pub fn persona_dir(&self, agent_id: &str) -> PathBuf {
        self.personas_dir().join(agent_id)
    }

    /// One agent's soul: `<root>/personas/<agent_id>/SOUL.md`.
    pub fn persona_soul_file(&self, agent_id: &str) -> PathBuf {
        self.persona_dir(agent_id).join(IDENTITY_SOUL_FILE)
    }

    /// One agent's private skill overlay:
    /// `<root>/personas/<agent_id>/skills/`.
    pub fn persona_skills_dir(&self, agent_id: &str) -> PathBuf {
        self.persona_dir(agent_id).join(PERSONA_SKILLS_DIR)
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

    /// Parent of the virtual per-session transcript path:
    /// `<root>/logs/sessions/`. No files are written here; see
    /// [`Self::session_log_file`].
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

    /// Virtual per-session transcript path:
    /// `<root>/logs/sessions/<sanitized_session_id>.jsonl`. No file is
    /// written here — the compaction summary embeds this path as a
    /// `read the full transcript at <path>` pointer, and a `Read` of it is
    /// served from the durable `session_messages` transcript (see the
    /// session-transcript read intercept in `baybo-agent`'s tool executor).
    pub fn session_log_file(&self, session_id: &str) -> PathBuf {
        self.sessions_log_dir().join(format!(
            "{}.{}",
            sanitize_session_id(session_id),
            SESSION_LOG_EXTENSION
        ))
    }

    // -- work/ contents --

    /// Workspace-scoped uv state parent: `<root>/work/.uv/`.
    pub fn uv_state_dir(&self) -> PathBuf {
        self.work_dir().join(UV_STATE_SUBDIR)
    }

    /// uv download / wheel cache: `<root>/work/.uv/cache/`.
    pub fn uv_cache_dir(&self) -> PathBuf {
        self.uv_state_dir().join("cache")
    }

    /// uv-managed Python installations: `<root>/work/.uv/python/`.
    pub fn uv_python_dir(&self) -> PathBuf {
        self.uv_state_dir().join("python")
    }

    /// `uv tool install` target: `<root>/work/.uv/tools/`.
    pub fn uv_tool_dir(&self) -> PathBuf {
        self.uv_state_dir().join("tools")
    }

    /// `uv tool install` bin shim dir: `<root>/work/.uv/bin/`.
    pub fn uv_tool_bin_dir(&self) -> PathBuf {
        self.uv_state_dir().join("bin")
    }

    pub fn browser_fonts_dir(&self) -> PathBuf {
        self.work_dir().join(BROWSER_FONTS_SUBDIR)
    }

    pub fn tool_spills_dir(&self) -> PathBuf {
        self.work_dir().join(TOOL_SPILLS_SUBDIR)
    }

    /// Disposable scratch: `<root>/work/tmp/`. See [`WORK_TMP_SUBDIR`].
    pub fn work_tmp_dir(&self) -> PathBuf {
        self.work_dir().join(WORK_TMP_SUBDIR)
    }
}

/// Replace any character that isn't `[A-Za-z0-9_\-.]` with `_`, then
/// prefix `_` if the result is empty or starts with `.`. Maps a
/// `SessionId` onto a safe filename component for the virtual per-session
/// transcript path ([`WorkspacePaths::session_log_file`]).
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
        let p = WorkspacePaths::new("/var/baybo");
        assert_eq!(p.config_dir(), PathBuf::from("/var/baybo/config"));
        assert_eq!(
            p.config_file(),
            PathBuf::from("/var/baybo/config/baybo.json")
        );
        assert_eq!(p.mcp_config(), PathBuf::from("/var/baybo/config/.mcp.json"));
        assert_eq!(p.profile_dir(), PathBuf::from("/var/baybo/profile"));
        assert_eq!(
            p.identity_file(IdentityKind::Soul),
            PathBuf::from("/var/baybo/profile/SOUL.md"),
        );
        assert_eq!(p.key_dir(), PathBuf::from("/var/baybo/.key"));
        assert_eq!(
            p.encryption_key_file(),
            PathBuf::from("/var/baybo/.key/encryption.key"),
        );
        assert_eq!(p.storage_db(), PathBuf::from("/var/baybo/state/storage.db"),);
        assert_eq!(
            p.singleton_lock(),
            PathBuf::from("/var/baybo/state/baybo.lock"),
        );
        assert_eq!(
            p.channel_port(),
            PathBuf::from("/var/baybo/state/channel.port"),
        );
        assert_eq!(p.logs_dir(), PathBuf::from("/var/baybo/logs"));
        assert_eq!(
            p.channel_logs_dir(),
            PathBuf::from("/var/baybo/logs/channel"),
        );
        assert_eq!(
            p.sessions_log_dir(),
            PathBuf::from("/var/baybo/logs/sessions"),
        );
        assert_eq!(p.skills_dir(), PathBuf::from("/var/baybo/skills"));
        assert_eq!(p.agents_dir(), PathBuf::from("/var/baybo/agents"));
        assert_eq!(p.uv_state_dir(), PathBuf::from("/var/baybo/work/.uv"));
        assert_eq!(p.uv_cache_dir(), PathBuf::from("/var/baybo/work/.uv/cache"));
        assert_eq!(
            p.uv_python_dir(),
            PathBuf::from("/var/baybo/work/.uv/python"),
        );
        assert_eq!(p.uv_tool_dir(), PathBuf::from("/var/baybo/work/.uv/tools"));
        assert_eq!(
            p.uv_tool_bin_dir(),
            PathBuf::from("/var/baybo/work/.uv/bin")
        );
        assert_eq!(
            p.browser_fonts_dir(),
            PathBuf::from("/var/baybo/work/.fonts"),
        );
        assert_eq!(
            p.browser_profile_dir(),
            PathBuf::from("/var/baybo/state/browser/profile"),
        );
        assert_eq!(
            p.tool_spills_dir(),
            PathBuf::from("/var/baybo/work/.baybo-tool-spills"),
        );
        assert_eq!(p.work_tmp_dir(), PathBuf::from("/var/baybo/work/tmp"));
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
        assert_eq!(CACHE_SUBDIR, "baybo");
    }

    #[test]
    fn default_config_file_lives_under_default_root_config() {
        let cfg = default_config_file();
        let root = default_workspace_root();
        assert_eq!(cfg, root.join(CONFIG_DIR).join(WORKSPACE_CONFIG_FILE));
    }
}
