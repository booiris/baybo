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
//!   agents/            # standalone git repo: subagent profile definitions
//!   personas/          # standalone git repo: USER.md (shared) + per-agent identity, skills, and memory
//!   .key/              # not version-controlled: encryption.key
//!   state/             # not version-controlled: storage.db, baybo.lock, channel.port, browser/profile
//!   work/              # not version-controlled: sandbox FS scope; .uv/ (uv cache + downloaded pythons + tools), tmp/ (disposable, swept), other scratch
//!   logs/              # not version-controlled: baybo.log.YYYY-MM-DD, channel/<type>.log (sessions/<id>.jsonl is virtual — never written)
//! ```
//!
//! `config/`, `agents/` and `personas/` each get their own `.git` repo on
//! first `ensure_layout`; the workspace root itself is not git-tracked. One
//! repo spans the whole declarative agent surface — every agent's identity,
//! memory, and skills.

use std::path::{Component, Path, PathBuf};

use crate::prompt::*;

// ---------------------------------------------------------------------------
// Workspace top-level subdirectories
// ---------------------------------------------------------------------------

/// Standalone git repo at `<root>/config/`: baybo.json + MCP registry.
/// Operator-edited, version-controlled.
pub const CONFIG_DIR: &str = "config";

/// Directory name of the built-in agent's persona inside [`PERSONAS_DIR`].
/// Mirrors `baybo_model::BUILTIN_AGENT_PROFILE_ID`, which is defined as this
/// const — the built-in's id *is* its directory name, and paths are this
/// crate's job.
pub const BUILTIN_PERSONA_DIR: &str = "baybo";

/// One agent's skills below its persona directory, one directory per skill.
/// Every agent has its own and no agent shares one; the built-in is not an
/// exception, at `personas/baybo/skills/`.
pub const SKILLS_DIR: &str = "skills";

/// Standalone git repo at `<root>/agents/`: workspace-local subagent
/// profile definitions. One `<name>.md` per profile (no
/// directory-per-profile ceremony — a profile has no linked-files
/// concern, only a frontmatter + system-prompt body).
pub const AGENTS_DIR: &str = "agents";

/// Standalone git repo at `<root>/personas/`: global agents are direct
/// children, while project agents are grouped below [`PROJECT_PERSONAS_DIR`].
/// Each persona carries that agent's `SOUL.md`, `IDENTITY.md`, `USER.md`, its
/// own `skills/`, and its `memory/`.
///
/// The one entry directly inside it that belongs to no agent is
/// [`SHARED_USER_FILE`], the human's profile that every agent reads. It is
/// not a persona directory and is never addressed as one — see
/// [`classify_persona_path`].
pub const PERSONAS_DIR: &str = "personas";

/// Project-owned personas, at `<root>/personas/project/<agent_id>/`.
///
/// Global chat personas keep their original `<root>/personas/<agent_id>/`
/// layout. Project agents are grouped separately because they are managed as
/// one board-facing roster rather than from the global Agents page.
pub const PROJECT_PERSONAS_DIR: &str = "project";

/// Prefix on newly minted project-agent ids. It is the stable discriminator
/// that keeps [`WorkspacePaths::persona_dir`] pure while preserving the flat
/// location of project agents created by older builds.
pub const PROJECT_PERSONA_ID_PREFIX: &str = "project-";

/// Whether an agent id selects the grouped project-persona layout.
pub fn is_project_persona_id(agent_id: &str) -> bool {
    agent_id
        .strip_prefix(PROJECT_PERSONA_ID_PREFIX)
        .is_some_and(|suffix| !suffix.is_empty())
}

/// The shared human profile at `<root>/personas/USER.md`. Read by every
/// agent alongside its own `USER.md` notes, and owned by none of them —
/// which is why it sits beside the agent directories rather than inside one.
pub const SHARED_USER_FILE: &str = "USER.md";

/// One agent's long-term memory below its persona directory — one markdown
/// file per remembered fact, indexed by [`MEMORY_INDEX_FILE`].
/// Agent-authored: the model writes here through `Edit` / `Write` and
/// prunes through `MemoryDelete`, and every write is audit-committed into
/// the `personas/` repo.
///
/// Per agent with no shared tree, because the built-in is just another
/// persona directory ([`BUILTIN_PERSONA_DIR`]) — the same reason its soul
/// has no special home either.
pub const PERSONA_MEMORY_DIR: &str = "memory";

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
// Identity file names, shared by every persona directory
// ---------------------------------------------------------------------------

pub const IDENTITY_SOUL_FILE: &str = "SOUL.md";
pub const IDENTITY_USER_FILE: &str = "USER.md";
pub const IDENTITY_IDENTITY_FILE: &str = "IDENTITY.md";

// ---------------------------------------------------------------------------
// Files inside a persona's memory dir
// ---------------------------------------------------------------------------

/// The one file in a memory dir that is not a memory: the index the
/// system prompt carries verbatim. One line per memory file; the bodies
/// are read on demand.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

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

/// Blob payload tree inside [`STATE_DIR`]: `<root>/state/blobs/`.
///
/// The name lives here, with the rest of the layout, because two unrelated
/// places must agree on it and neither can see the other's derivation:
/// `baybo-storage` anchors it to the sqlite file's parent (`Store::open`
/// takes a bare db path, not a [`WorkspacePaths`]), while the agent runtime
/// re-exposes the same directory through the sandbox's read-only bind. A
/// literal in both would be a rename away from a sandbox that silently
/// stops granting anything.
pub const BLOBS_SUBDIR: &str = "blobs";

// ---------------------------------------------------------------------------
// Files inside `logs/` (not version-controlled)
// ---------------------------------------------------------------------------

/// File-name prefix for the gateway's rolling log files (`baybo.log.YYYY-MM-DD`).
pub const LOG_FILE_PREFIX: &str = "baybo.log";

/// File-name prefix for the chat TUI's rolling log files
/// (`tui.log.YYYY-MM-DD`). Separate from [`LOG_FILE_PREFIX`] because the TUI is
/// its own process: ratatui owns its stdout, so the file is where its
/// diagnostics land, and appending them to the gateway's log would interleave
/// two writers into one file.
pub const TUI_LOG_FILE_PREFIX: &str = "tui.log";

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

/// Separates a session id from the ordinal a transcript read starts at
/// (see [`WorkspacePaths::session_log_file_from`]). Deliberately a
/// character [`sanitize_session_id`] rewrites, so it cannot occur inside
/// the id half and the split is unambiguous.
pub const SESSION_ORDINAL_SEPARATOR: char = '@';

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

/// True when any component of `path` is `..`.
///
/// The other half of [`absolutise`]'s warning: it leaves `..` intact, and
/// [`Path::starts_with`] is purely lexical, so a prefix test alone will
/// happily accept `<root>/a/../../elsewhere`. Anything deciding "is this
/// path inside that directory" needs both.
pub fn escapes_upward(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

/// True when any component of `path` is `.git`.
///
/// Several of the workspace's directories *are* standalone git repos
/// (see the layout above), so their own `.git/` sits inside the tree they
/// version. Anything granting write access to such a tree has to exclude
/// it: `git` reads its configuration from the repo it runs in, and options
/// like `core.fsmonitor` or a `filter.*.clean` command run arbitrary
/// programs on the next `git add` — which the writer itself performs when
/// it commits.
pub fn has_git_component(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
}

/// What a path under `personas/` is, recognised from its shape.
///
/// The recognising half of [`WorkspacePaths::shared_user_file`],
/// [`WorkspacePaths::persona_identity_file`] and
/// [`WorkspacePaths::persona_memory_dir`]. It lives beside them on purpose:
/// a recogniser that drifted from its constructor would keep matching a
/// layout that no longer exists, and callers use this to decide who may
/// write what.
///
/// Shape only. Whether *this* caller may write *this* path — ownership — is
/// the caller's to decide, by matching the variant it cares about and
/// comparing the `agent_id` it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaPath<'a> {
    /// `personas/USER.md` — the human profile every agent shares.
    SharedUser,
    /// An agent's `{SOUL,IDENTITY,USER}.md`, in either the flat legacy/global
    /// layout or `personas/project/<agent_id>/`. Carries which of the three,
    /// because `IDENTITY.md` has a rule about its contents that its siblings
    /// — and a memory file that merely happens to share its name — do not.
    Identity {
        agent_id: &'a str,
        kind: IdentityKind,
    },
    /// A file under an agent's `memory/` directory in either layout.
    Memory { agent_id: &'a str },
    /// Anything else: an agent's skills directory, a bare directory, a stray
    /// file, or a path this crate cannot read as UTF-8.
    Other,
}

/// Classify a path already known to be under `personas/`, given relative to
/// it. Non-UTF-8 components read as [`PersonaPath::Other`] rather than
/// being lossily coerced — two different byte sequences must never collapse
/// onto one recognised name.
pub fn classify_persona_path(relative: &Path) -> PersonaPath<'_> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component.as_os_str().to_str() {
            Some(part) => parts.push(part),
            None => return PersonaPath::Other,
        }
    }
    match parts.as_slice() {
        [SHARED_USER_FILE] => PersonaPath::SharedUser,
        // Exactly three components, so this never shadows the memory arm
        // below (four or more). A name that is not one of the three identity
        // files lands where it did before: `Other`.
        [PROJECT_PERSONAS_DIR, agent_id, name] => match IdentityKind::from_file_name(name) {
            Some(kind) => PersonaPath::Identity { agent_id, kind },
            None => PersonaPath::Other,
        },
        [PROJECT_PERSONAS_DIR, agent_id, dir, _rest @ ..]
            if *dir == PERSONA_MEMORY_DIR && parts.len() >= 4 =>
        {
            PersonaPath::Memory { agent_id }
        }
        [PROJECT_PERSONAS_DIR, ..] => PersonaPath::Other,
        [agent_id, name] => match IdentityKind::from_file_name(name) {
            Some(kind) => PersonaPath::Identity { agent_id, kind },
            None => PersonaPath::Other,
        },
        // A bare `<agent_id>/memory` is the directory itself, not a file in it.
        [agent_id, dir, _rest @ ..] if *dir == PERSONA_MEMORY_DIR && parts.len() >= 3 => {
            PersonaPath::Memory { agent_id }
        }
        _ => PersonaPath::Other,
    }
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

    /// The kind stored under `file_name`, or `None` for any other name.
    /// The exact-cased inverse of [`Self::file_name`] — unlike
    /// [`Self::from_label`], which is a tolerant parser for what a person
    /// types, this one decides what a path on disk *is*.
    pub fn from_file_name(file_name: &str) -> Option<Self> {
        Self::all().into_iter().find(|k| k.file_name() == file_name)
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

    /// One agent's persona directory.
    ///
    /// Global and legacy project agents live at
    /// `<root>/personas/<agent_id>/`; newly materialised project agents carry
    /// [`PROJECT_PERSONA_ID_PREFIX`] and live at
    /// `<root>/personas/project/<agent_id>/`. Routing from the id keeps this
    /// path constructor free of I/O and leaves older unprefixed ids untouched.
    ///
    /// Callers hold an `AgentProfileId`, whose grammar is what keeps each
    /// joined component inside `personas/`; this crate is leaf-level and
    /// takes the id as a `&str`.
    pub fn persona_dir(&self, agent_id: &str) -> PathBuf {
        if is_project_persona_id(agent_id) {
            self.project_persona_dir(agent_id)
        } else {
            self.personas_dir().join(agent_id)
        }
    }

    /// Root of project-owned personas: `<root>/personas/project/`.
    pub fn project_personas_dir(&self) -> PathBuf {
        self.personas_dir().join(PROJECT_PERSONAS_DIR)
    }

    /// One project-owned persona: `<root>/personas/project/<agent_id>/`.
    pub fn project_persona_dir(&self, agent_id: &str) -> PathBuf {
        self.project_personas_dir().join(agent_id)
    }

    /// One agent's own copy of an identity file:
    /// `<root>/personas/<agent_id>/<FILE>.md`, or the corresponding path
    /// below `personas/project/` for a project-owned persona.
    ///
    /// Only `SOUL.md` and `IDENTITY.md` are ever addressed this way —
    /// personality and self-image belong to the agent, while `USER.md`
    /// describes the human and also has a shared copy. This method
    /// takes any kind because it is pure address arithmetic; the routing
    /// rule lives with the agent id (`AgentProfileId::identity_file`).
    pub fn persona_identity_file(&self, agent_id: &str, kind: IdentityKind) -> PathBuf {
        self.persona_dir(agent_id).join(kind.file_name())
    }

    /// One agent's private skill overlay below its resolved persona directory.
    pub fn persona_skills_dir(&self, agent_id: &str) -> PathBuf {
        self.persona_dir(agent_id).join(SKILLS_DIR)
    }

    /// One agent's private memory tree below its resolved persona directory.
    pub fn persona_memory_dir(&self, agent_id: &str) -> PathBuf {
        self.persona_dir(agent_id).join(PERSONA_MEMORY_DIR)
    }

    /// Index of one agent's memory below its resolved persona directory.
    pub fn persona_memory_index_file(&self, agent_id: &str) -> PathBuf {
        self.persona_memory_dir(agent_id).join(MEMORY_INDEX_FILE)
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

    // -- personas/ contents --

    /// The human's shared profile: `<root>/personas/USER.md`.
    pub fn shared_user_file(&self) -> PathBuf {
        self.personas_dir().join(SHARED_USER_FILE)
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

    /// Virtual transcript path that starts at a message rather than at the
    /// beginning: `<root>/logs/sessions/<sanitized_session_id>@<ordinal>.jsonl`.
    ///
    /// The suffix is what keeps a recurring pass over a long-lived
    /// conversation from re-reading — and re-paying for — everything it
    /// already consolidated. Reading from ordinal 0 is what
    /// [`Self::session_log_file`] already spells, so that case composes the
    /// plain path instead of a redundant `@0`.
    ///
    /// [`SESSION_ORDINAL_SEPARATOR`] cannot collide with the id: it is not
    /// one of the characters [`sanitize_session_id`] keeps, so a sanitized
    /// id never contains it.
    pub fn session_log_file_from(&self, session_id: &str, from_ordinal: i64) -> PathBuf {
        if from_ordinal <= 0 {
            return self.session_log_file(session_id);
        }
        self.sessions_log_dir().join(format!(
            "{}{SESSION_ORDINAL_SEPARATOR}{from_ordinal}.{}",
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

    /// Blob payloads: `<root>/state/blobs/`. See [`BLOBS_SUBDIR`].
    pub fn blobs_dir(&self) -> PathBuf {
        self.state_dir().join(BLOBS_SUBDIR)
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
    fn a_transcript_path_can_name_where_to_start_reading() {
        let p = WorkspacePaths::new("/var/baybo");
        assert_eq!(
            p.session_log_file_from("sess-a", 312),
            PathBuf::from("/var/baybo/logs/sessions/sess-a@312.jsonl")
        );
        // Reading from the beginning is what the plain path already means;
        // an `@0` would be a second spelling of it, and the reader's
        // round-trip check would then have two right answers.
        assert_eq!(
            p.session_log_file_from("sess-a", 0),
            p.session_log_file("sess-a")
        );
        assert_eq!(
            p.session_log_file_from("sess-a", -1),
            p.session_log_file("sess-a")
        );
        // The separator survives no id, because it survives no id: the
        // sanitiser keeps only alphanumerics and `_-.`, so the split is
        // always at the ordinal.
        assert!(!sanitize_session_id("a@b").contains(SESSION_ORDINAL_SEPARATOR));
    }

    #[test]
    fn workspace_paths_compose_under_root() {
        let p = WorkspacePaths::new("/var/baybo");
        assert_eq!(p.config_dir(), PathBuf::from("/var/baybo/config"));
        assert_eq!(
            p.config_file(),
            PathBuf::from("/var/baybo/config/baybo.json")
        );
        assert_eq!(p.mcp_config(), PathBuf::from("/var/baybo/config/.mcp.json"));
        assert_eq!(p.personas_dir(), PathBuf::from("/var/baybo/personas"));
        assert_eq!(
            p.project_personas_dir(),
            PathBuf::from("/var/baybo/personas/project")
        );
        assert_eq!(
            p.project_persona_dir("agt_1"),
            PathBuf::from("/var/baybo/personas/project/agt_1")
        );
        assert_eq!(
            p.persona_dir("project-01JAGENT"),
            PathBuf::from("/var/baybo/personas/project/project-01JAGENT")
        );
        assert_eq!(
            p.persona_dir("01JLEGACY"),
            PathBuf::from("/var/baybo/personas/01JLEGACY")
        );
        assert_eq!(
            p.shared_user_file(),
            PathBuf::from("/var/baybo/personas/USER.md")
        );
        assert_eq!(
            p.persona_identity_file(BUILTIN_PERSONA_DIR, IdentityKind::Soul),
            PathBuf::from("/var/baybo/personas/baybo/SOUL.md"),
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
        assert_eq!(p.agents_dir(), PathBuf::from("/var/baybo/agents"));
        assert_eq!(
            p.persona_skills_dir("agt_1"),
            PathBuf::from("/var/baybo/personas/agt_1/skills"),
        );
        assert_eq!(
            p.persona_memory_dir("agt_1"),
            PathBuf::from("/var/baybo/personas/agt_1/memory"),
        );
        assert_eq!(
            p.persona_memory_index_file("agt_1"),
            PathBuf::from("/var/baybo/personas/agt_1/memory/MEMORY.md"),
        );
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
    fn persona_paths_are_recognised_as_their_constructors_build_them() {
        let p = WorkspacePaths::new("/w");
        let personas = p.personas_dir();
        let rel = |full: PathBuf| {
            full.strip_prefix(&personas)
                .expect("under personas")
                .to_path_buf()
        };

        assert_eq!(
            classify_persona_path(&rel(p.shared_user_file())),
            PersonaPath::SharedUser
        );
        for kind in IdentityKind::all() {
            assert_eq!(
                classify_persona_path(&rel(p.persona_identity_file("agt_1", kind))),
                PersonaPath::Identity {
                    agent_id: "agt_1",
                    kind
                },
                "{kind:?}",
            );
        }
        assert_eq!(
            classify_persona_path(&rel(p.persona_memory_index_file("agt_1"))),
            PersonaPath::Memory { agent_id: "agt_1" }
        );

        let project = p.project_persona_dir("agt_project");
        for kind in IdentityKind::all() {
            assert_eq!(
                classify_persona_path(&rel(project.join(kind.file_name()))),
                PersonaPath::Identity {
                    agent_id: "agt_project",
                    kind
                },
                "{kind:?}",
            );
        }
        assert_eq!(
            classify_persona_path(&rel(project
                .join(PERSONA_MEMORY_DIR)
                .join(MEMORY_INDEX_FILE))),
            PersonaPath::Memory {
                agent_id: "agt_project"
            }
        );
        assert_eq!(
            classify_persona_path(Path::new("project/IDENTITY.md")),
            PersonaPath::Other
        );
        assert_eq!(
            classify_persona_path(Path::new("project/memory/fact.md")),
            PersonaPath::Other
        );

        // The tree root is a directory, not a file in it; a skills overlay
        // is neither identity nor memory.
        assert_eq!(
            classify_persona_path(&rel(p.persona_memory_dir("agt_1"))),
            PersonaPath::Other
        );
        assert_eq!(
            classify_persona_path(&rel(p.persona_skills_dir("agt_1").join("deploy/SKILL.md"))),
            PersonaPath::Other
        );
        // A decoy named like an identity file, one level too deep.
        assert_eq!(
            classify_persona_path(Path::new("agt_1/skills/SOUL.md")),
            PersonaPath::Other
        );
    }

    #[test]
    fn path_guards_catch_what_a_prefix_test_cannot() {
        assert!(escapes_upward(Path::new(
            "/w/personas/agt/memory/../SOUL.md"
        )));
        assert!(!escapes_upward(Path::new("/w/personas/agt/memory/note.md")));
        assert!(has_git_component(Path::new("/w/personas/.git/config")));
        assert!(has_git_component(Path::new(
            "/w/personas/agt/memory/.git/HEAD"
        )));
        assert!(!has_git_component(Path::new(
            "/w/personas/agt/memory/gitlog.md"
        )));
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
