# workspace - Workspace and Long-Running Configuration

## Overview

The `workspace` crate manages Aura's persistent workspace and long-running configuration. It stores "identity and strategy," not ordinary conversation memory.

Identity files: `AGENTS.md`, `SOUL.md`, `USER.md`, `IDENTITY.md`.

It provides `agent` with long-term personality and identity injection.

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

- No workspace crate dependencies
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
