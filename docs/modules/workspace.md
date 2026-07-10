# workspace - Workspace Addresses and Long-Running Configuration

## Overview

The `workspace` crate is the single source of truth for Baybo's workspace layout. It owns:

- **Filesystem addresses** (`paths` module, always available): `WorkspacePaths`, `IdentityKind`, the `&str` constants for every workspace-relative file/dir name (`profile/`, `skills/`, `state/`, `work/`, `logs/`, `baybo.json`, `.mcp.json`, `storage.db`, `baybo.lock`, `channel.port`, `SOUL.md` / `USER.md` / `IDENTITY.md`, `.uv/`), the `ENV_CONFIG_PATH` constant (whose value is the env-var name `BAYBO_CONFIG_PATH`), and the `default_workspace_root` / `default_config_file` / `baybo_cache_root` resolvers.
- **Identity I/O** (`io` feature, default-on): `WorkspaceManager`, `IdentityFiles`, `load_identity_files`, `write_identity_file`, `WorkspaceManager::ensure_layout` — the async readers/writers backing the three identity documents and the workspace-skeleton initializer.
- **Default identity templates** (`prompt` module, always available): the `DEFAULT_SOUL_CONTENT` / `DEFAULT_USER_CONTENT` / `DEFAULT_IDENTITY_CONTENT` seed strings that `IdentityKind::default_content` returns when `seed_default_identity_files` writes a missing `SOUL.md` / `USER.md` / `IDENTITY.md`.

Pure-data consumers (e.g. `baybo-config`, `baybo-tools`) take this crate with `default-features = false` so they never inherit a transitive `tokio`/`anyhow` dependency just to read a path constant. Crates that actually drive workspace I/O (`baybo-agent`, `baybo-cli`, `baybo-gateway`, the binary) depend on it with `features = ["io"]`.

## Layout

The workspace root is the single **project root** for the entire runtime: every subsystem that needs a persistent path derives its location from it. The root is divided into eight top-level subdirectories that describe what kind of content lives there.

```text
<workspace_root>/
  config/          # standalone git repo: baybo.json, .mcp.json
  profile/         # standalone git repo: identity .md files
  skills/          # standalone git repo: workspace-local skill definitions
  agents/          # standalone git repo: subagent profile definitions
  agent-skills/    # standalone git repo: per-agent-profile skill definitions, one subdir per agent id
  state/           # not version-controlled: storage.db, baybo.lock, channel.port, browser/profile
  work/            # not version-controlled: .uv/ (uv cache + downloaded pythons + tools), future scratch
  logs/            # not version-controlled: baybo.log.<date>, channel/<type>.log.<date> (sessions/<id>.jsonl is a virtual path, never written)
```

| Field            | Default | Role                                        |
| ---------------- | ------- | ------------------------------------------- |
| `workspace.path` | `default_workspace_root()` | Project root directory (validated non-empty)|

Defaults: `~/.baybo` in release builds, `./.baybo` in debug builds. The
debug default keeps `cargo run` self-contained inside the project
checkout rather than polluting the real user home.

| Subsystem        | Path                                       |
| ---------------- | ------------------------------------------ |
| config           | `<workspace.path>/config/baybo.json`        |
| MCP servers      | `<workspace.path>/config/.mcp.json`        |
| identity files   | `<workspace.path>/profile/{SOUL,USER,IDENTITY}.md` |
| skills           | `<workspace.path>/skills/`                 |
| agent skills     | `<workspace.path>/agent-skills/<agent_id>/<skill>/SKILL.md` |
| encryption key   | `<workspace.path>/.key/encryption.key`     |
| storage          | `<workspace.path>/state/storage.db`        |
| singleton lock   | `<workspace.path>/state/baybo.lock`         |
| channel port     | `<workspace.path>/state/channel.port`      |
| browser profile  | `<workspace.path>/state/browser/profile/`  |
| uv state         | `<workspace.path>/work/.uv/{cache,python,tools,bin}/` |
| gateway logs     | `<workspace.path>/logs/baybo.log.<date>`    |
| channel logs     | `<workspace.path>/logs/channel/<channel_type>.log.<date>` |
| session transcript (virtual) | `<workspace.path>/logs/sessions/<session_id>.jsonl` (no file; the compaction recovery pointer, served from `session_messages` on read) |

New subsystem files belong as a method on `WorkspacePaths`, not as another `workspace_root.join("…")` call site.

## Initialization and version control

`WorkspaceManager::ensure_layout` runs at every boot (gateway start, TUI, argv subcommands once `boot::load_config` returns) and is idempotent:

- Creates `config/`, `profile/`, `skills/`, `agents/`, `agent-skills/`, `.key/`, `state/`, `work/`, `logs/` if missing.
- Runs `git init --quiet` inside `config/`, `profile/`, `skills/`, `agents/`, and `agent-skills/` if the directory isn't already a git repo (`<dir>/.git` check).

`config/`, `profile/`, `skills/`, `agents/`, and `agent-skills/` are each their own standalone git repo. The workspace root itself is **not** version-controlled — there is no top-level `.gitignore`, and `.key/`, `state/`, `work/`, `logs/` simply live next to the five declarative dirs without needing an ignore list to keep them out of any tree above them. Users who want to back up or sync their config commit inside `config/`; identity edits commit inside `profile/`; skill authors do the same inside `skills/`; subagent profiles commit inside `agents/`; per-agent skill authors commit inside `agent-skills/`. **Never** commit anything from `.key/` — `baybo setup` mints the master encryption key there with mode 0600, and treating that file as version-controllable would leak every secret in the vault.

## Config file resolution

The `ENV_CONFIG_PATH` constant holds the env-var name `BAYBO_CONFIG_PATH`; setting that env var overrides everything. When unset, the loader falls back to `default_config_file()` = `<default_workspace_root>/config/baybo.json`. Missing-file behaviour:

- explicit `BAYBO_CONFIG_PATH` pointing at a non-existent file → hard error.
- default path absent → silently fall back to `BayboConfig::default()` and log the resolved path that was checked.

## Design Decisions

### Identity file responsibilities

- **SOUL.md**: personality, tone, and preferences
- **USER.md**: long-term user profile
- **IDENTITY.md**: system or instance identity description

Identity file changes usually affect the system prompt; memory changes usually affect recall.

### Boundary with memory

- **workspace**: persistent identity and strategy files
- **memory**: retrievable, recallable, and expirable user memory

They complement each other without overlapping.

### Why split state/work/logs from the declarative dirs

The split exists so the user's git workflow stays clean: `config/`, `profile/`, `skills/`, `agents/`, and `agent-skills/` are declarative, hand-edited (or hand-authored-per-agent) content that belongs in source control; `state/` is mutable runtime state (libsql DB, locks, ports, browser profile) that would create churn or conflicts if committed; `work/` holds tool-generated scratch (uv caches, downloaded Python toolchains, ad-hoc shell output) that has no long-term value; `logs/` is ephemeral. Each declarative dir is its own git repo, so the boundary is enforced by repo scope rather than a top-level ignore list — users can never accidentally commit `state/` because no enclosing repo includes it.

## Constraints

- No baybo-* dependencies. The crate is leaf-level: `paths` is pure data, and `io` only adds optional `tokio` + `anyhow`.
- Does not record message-level Trace or Job data
- Missing identity files should degrade gracefully, not block startup
- Identity file changes should carry a version stamp or content hash for Trace provenance

## Collaboration

| Module | Role |
|--------|------|
| `agent` | Reads identity files for system prompt |
| `skills` | Provides trusted local skill directories, including `agent-skills/` for the per-agent overlay ([`skills.md`](skills.md#agent-scoped-overlay)) |
| `trace` | Identity file version changes are recorded in provenance |
| `agent-profiles` | `agent-skills/<agent_id>/` is keyed by profile id ([`agent-profiles.md`](agent-profiles.md#session-binding)) |
| `memory` | Complements without overlapping responsibility |
