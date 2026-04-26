# workspace - Workspace Addresses and Long-Running Configuration

## Overview

The `workspace` crate is the single source of truth for Aura's workspace layout. It owns:

- **Filesystem addresses** (`paths` module, always available): `WorkspacePaths`, `IdentityKind`, the `&str` constants for every workspace-relative file/dir name (`storage.db`, `aura.lock`, `channel.port`, `.mcp.json`, `AGENTS.md` / `SOUL.md` / `USER.md` / `IDENTITY.md`, `logs/`, `skills/`, `.aura/code-builder/runs/`), the `AURA_CONFIG_PATH` env-var name, and the `default_workspace_root` / `aura_cache_root` resolvers.
- **Identity I/O** (`io` feature, default-on): `WorkspaceManager`, `IdentityFiles`, `load_identity_files`, `write_identity_file` — the async readers/writers backing the four identity documents.

Pure-data consumers (e.g. `aura-config`, `aura-tools`, `aura-code-builder`) take this crate with `default-features = false` so they never inherit a transitive `tokio`/`anyhow` dependency just to read a path constant. Crates that actually drive workspace I/O (`aura-agent`, `aura-cli`, `aura-gateway`, the binary) depend on it with `features = ["io"]`.

## Configuration

The workspace root is the single **project root** for the entire runtime: every subsystem that needs a persistent path (workspace identity files, libsql storage, etc.) derives its location from it. There is no separate knob for storage or data paths.

| Field            | Default | Role                                        |
| ---------------- | ------- | ------------------------------------------- |
| `workspace.path` | `"."`   | Project root directory (validated non-empty)|

Convention for derived paths (all resolved through `paths::WorkspacePaths`):

| Subsystem      | Path                                       |
| -------------- | ------------------------------------------ |
| storage        | `<workspace.path>/storage.db`              |
| singleton lock | `<workspace.path>/aura.lock`               |
| channel port   | `<workspace.path>/channel.port`            |
| MCP servers    | `<workspace.path>/.mcp.json`               |
| logs           | `<workspace.path>/logs/aura.log.<date>`    |
| skills         | `<workspace.path>/skills/`                 |
| code-builder   | `<workspace.path>/.aura/code-builder/runs/<uuid>/` |
| identity files | `<workspace.path>/{AGENTS,SOUL,USER,IDENTITY}.md` |

New subsystem files belong as a method on `WorkspacePaths`, not as another `workspace_root.join("…")` call site.

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
