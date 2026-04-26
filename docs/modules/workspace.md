# workspace - Workspace Addresses and Long-Running Configuration

## Overview

The `workspace` crate is the single source of truth for Aura's workspace layout. It owns:

- **Filesystem addresses** (`paths` module, always available): `WorkspacePaths`, `IdentityKind`, the `&str` constants for every workspace-relative file/dir name (`profile/`, `skills/`, `state/`, `work/`, `logs/`, `aura.json`, `.mcp.json`, `storage.db`, `aura.lock`, `channel.port`, `AGENTS.md` / `SOUL.md` / `USER.md` / `IDENTITY.md`, `.gitignore`, `code-builder/runs/`), the `AURA_CONFIG_PATH` env-var name, and the `default_workspace_root` / `default_config_file` / `aura_cache_root` resolvers.
- **Identity I/O** (`io` feature, default-on): `WorkspaceManager`, `IdentityFiles`, `load_identity_files`, `write_identity_file`, `WorkspaceManager::ensure_layout` — the async readers/writers backing the four identity documents and the workspace-skeleton initializer.

Pure-data consumers (e.g. `aura-config`, `aura-tools`, `aura-code-builder`) take this crate with `default-features = false` so they never inherit a transitive `tokio`/`anyhow` dependency just to read a path constant. Crates that actually drive workspace I/O (`aura-agent`, `aura-cli`, `aura-gateway`, the binary) depend on it with `features = ["io"]`.

## Layout

The workspace root is the single **project root** for the entire runtime: every subsystem that needs a persistent path derives its location from it. The root is divided into four top-level subdirectories that describe what kind of content lives there.

```text
<workspace_root>/
  .gitignore       # allowlists profile/ and skills/
  profile/         # git-tracked: aura.json, .mcp.json, identity .md files
  skills/          # git-tracked: workspace-local skill definitions
  state/           # ignored: storage.db, aura.lock, channel.port
  work/            # ignored: code-builder/runs/<uuid>/, future scratch
  logs/            # ignored: aura.log.YYYY-MM-DD
```

| Field            | Default | Role                                        |
| ---------------- | ------- | ------------------------------------------- |
| `workspace.path` | `default_workspace_root()` | Project root directory (validated non-empty)|

Defaults: `~/.aura` in release builds, `./.aura` in debug builds. The
debug default keeps `cargo run` self-contained inside the project
checkout rather than polluting the real user home.

| Subsystem      | Path                                       |
| -------------- | ------------------------------------------ |
| config         | `<workspace.path>/profile/aura.json`       |
| MCP servers    | `<workspace.path>/profile/.mcp.json`       |
| identity files | `<workspace.path>/profile/{AGENTS,SOUL,USER,IDENTITY}.md` |
| skills         | `<workspace.path>/skills/`                 |
| storage        | `<workspace.path>/state/storage.db`        |
| singleton lock | `<workspace.path>/state/aura.lock`         |
| channel port   | `<workspace.path>/state/channel.port`      |
| code-builder   | `<workspace.path>/work/code-builder/runs/<uuid>/` |
| gateway logs   | `<workspace.path>/logs/aura.log.<date>`    |
| channel logs   | `<workspace.path>/logs/channel/<channel_type>.log.<date>` |

New subsystem files belong as a method on `WorkspacePaths`, not as another `workspace_root.join("…")` call site.

## Initialization and `.gitignore`

`WorkspaceManager::ensure_layout` runs at every boot (gateway start, TUI, argv subcommands once `boot::load_config` returns) and is idempotent:

- Creates `profile/`, `skills/`, `state/`, `work/`, `logs/` if missing.
- Writes a default `.gitignore` at the workspace root the first time only — existing files are never overwritten so users can hand-edit the allowlist.

The default `.gitignore` is allowlist-style:

```gitignore
/*
!/.gitignore
!/profile/
!/skills/
```

That keeps `state/`, `work/`, and `logs/` (and any other future runtime
directories) out of source control automatically — the user can `git init`
at the workspace root and only their `profile/` and `skills/` content gets
tracked.

## Config file resolution

`AURA_CONFIG_PATH` overrides everything. When unset, the loader falls back to `default_config_file()` = `<default_workspace_root>/profile/aura.json`. Missing-file behaviour:

- explicit `AURA_CONFIG_PATH` pointing at a non-existent file → hard error.
- default path absent → silently fall back to `AuraConfig::default()` and log the resolved path that was checked.

## Design Decisions

### Identity file responsibilities

- **AGENTS.md**: runtime constraints, roles, and high-level rules
- **SOUL.md**: personality, tone, and preferences
- **USER.md**: long-term user profile
- **IDENTITY.md**: system or instance identity description

Identity file changes usually affect the system prompt; memory changes usually affect recall.

### Boundary with memory

- **workspace**: persistent identity and strategy files
- **memory**: retrievable, recallable, and expirable user memory

They complement each other without overlapping.

### Why split state/work/logs from profile/skills

The split exists so the user's git workflow stays clean: `profile/` and `skills/` are declarative, hand-edited content that belongs in source control; `state/` is mutable runtime state (libsql DB, locks, ports) that would create churn or conflicts if committed; `work/` holds tool-generated scratch (code-builder runs) that has no long-term value; `logs/` is ephemeral. The `.gitignore` allowlist enforces the boundary by default — no per-file ignore drift.

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
