# workspace - Workspace Addresses and Long-Running Configuration

## Overview

The `workspace` crate is the single source of truth for Aura's workspace layout. It owns:

- **Filesystem addresses** (`paths` module, always available): `WorkspacePaths`, `IdentityKind`, the `&str` constants for every workspace-relative file/dir name (`profile/`, `skills/`, `state/`, `work/`, `logs/`, `aura.json`, `.mcp.json`, `storage.db`, `aura.lock`, `channel.port`, `SOUL.md` / `USER.md` / `IDENTITY.md`, `code-builder/runs/`), the `AURA_CONFIG_PATH` env-var name, and the `default_workspace_root` / `default_config_file` / `aura_cache_root` resolvers.
- **Identity I/O** (`io` feature, default-on): `WorkspaceManager`, `IdentityFiles`, `load_identity_files`, `write_identity_file`, `WorkspaceManager::ensure_layout` — the async readers/writers backing the three identity documents and the workspace-skeleton initializer.

Pure-data consumers (e.g. `aura-config`, `aura-tools`, `aura-code-builder`) take this crate with `default-features = false` so they never inherit a transitive `tokio`/`anyhow` dependency just to read a path constant. Crates that actually drive workspace I/O (`aura-agent`, `aura-cli`, `aura-gateway`, the binary) depend on it with `features = ["io"]`.

## Layout

The workspace root is the single **project root** for the entire runtime: every subsystem that needs a persistent path derives its location from it. The root is divided into four top-level subdirectories that describe what kind of content lives there.

```text
<workspace_root>/
  profile/         # standalone git repo: aura.json, .mcp.json, identity .md files
  skills/          # standalone git repo: workspace-local skill definitions
  state/           # not version-controlled: storage.db, aura.lock, channel.port, browser/profile
  work/            # not version-controlled: code-builder/runs/<uuid>/, future scratch
  logs/            # not version-controlled: aura.log.<date>, channel/<type>.log.<date>, sessions/<id>.jsonl
```

| Field            | Default | Role                                        |
| ---------------- | ------- | ------------------------------------------- |
| `workspace.path` | `default_workspace_root()` | Project root directory (validated non-empty)|

Defaults: `~/.aura` in release builds, `./.aura` in debug builds. The
debug default keeps `cargo run` self-contained inside the project
checkout rather than polluting the real user home.

| Subsystem        | Path                                       |
| ---------------- | ------------------------------------------ |
| config           | `<workspace.path>/config/aura.json`        |
| MCP servers      | `<workspace.path>/config/.mcp.json`        |
| identity files   | `<workspace.path>/profile/{SOUL,USER,IDENTITY}.md` |
| skills           | `<workspace.path>/skills/`                 |
| encryption key   | `<workspace.path>/.key/encryption.key`     |
| storage          | `<workspace.path>/state/storage.db`        |
| singleton lock   | `<workspace.path>/state/aura.lock`         |
| channel port     | `<workspace.path>/state/channel.port`      |
| browser profile  | `<workspace.path>/state/browser/profile/`  |
| code-builder     | `<workspace.path>/work/code-builder/runs/<uuid>/` |
| gateway logs     | `<workspace.path>/logs/aura.log.<date>`    |
| channel logs     | `<workspace.path>/logs/channel/<channel_type>.log.<date>` |
| session logs     | `<workspace.path>/logs/sessions/<session_id>.jsonl` |

New subsystem files belong as a method on `WorkspacePaths`, not as another `workspace_root.join("…")` call site.

## Initialization and version control

`WorkspaceManager::ensure_layout` runs at every boot (gateway start, TUI, argv subcommands once `boot::load_config` returns) and is idempotent:

- Creates `config/`, `profile/`, `skills/`, `.key/`, `state/`, `work/`, `logs/` if missing.
- Runs `git init --quiet` inside `config/`, `profile/`, and `skills/` if the directory isn't already a git repo (`<dir>/.git` check).

`config/`, `profile/`, and `skills/` are each their own standalone git repo. The workspace root itself is **not** version-controlled — there is no top-level `.gitignore`, and `.key/`, `state/`, `work/`, `logs/` simply live next to the three declarative dirs without needing an ignore list to keep them out of any tree above them. Users who want to back up or sync their config commit inside `config/`; identity edits commit inside `profile/`; skill authors do the same inside `skills/`. **Never** commit anything from `.key/` — `aura setup` mints the master encryption key there with mode 0600, and treating that file as version-controllable would leak every secret in the vault.

## Config file resolution

`AURA_CONFIG_PATH` overrides everything. When unset, the loader falls back to `default_config_file()` = `<default_workspace_root>/config/aura.json`. Missing-file behaviour:

- explicit `AURA_CONFIG_PATH` pointing at a non-existent file → hard error.
- default path absent → silently fall back to `AuraConfig::default()` and log the resolved path that was checked.

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

### Why split state/work/logs from profile/skills

The split exists so the user's git workflow stays clean: `profile/` and `skills/` are declarative, hand-edited content that belongs in source control; `state/` is mutable runtime state (libsql DB, locks, ports, browser profile) that would create churn or conflicts if committed; `work/` holds tool-generated scratch (code-builder runs) that has no long-term value; `logs/` is ephemeral. Each of the two declarative dirs is its own git repo, so the boundary is enforced by repo scope rather than a top-level ignore list — users can never accidentally commit `state/` because no enclosing repo includes it.

## Constraints

- No aura-* dependencies. The crate is leaf-level: `paths` is pure data, and `io` only adds optional `tokio` + `anyhow`.
- Does not record message-level Trace or Job data
- Missing identity files should degrade gracefully, not block startup
- Identity file changes should carry a version stamp or content hash for Trace provenance

## Collaboration

| Module | Role |
|--------|------|
| `agent` | Reads identity files for system prompt |
| `skills` | Provides trusted local skill directories |
| `trace` | Identity file version changes are recorded in provenance |
| `memory` | Complements without overlapping responsibility |
