# workspace - Workspace and Long-Running Configuration

## 1. Module Overview

The `workspace` crate is responsible for Aura's persistent workspace and long-running configuration. It does not store ordinary conversation memory. Instead, it stores "identity and strategy":

- `AGENTS.md`
- `SOUL.md`
- `USER.md`
- `IDENTITY.md`
- `HEARTBEAT.md`

It provides `agent` with long-term personality, identity injection, and the configuration source for heartbeat and routines.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose                              |
| ---------------- | ------------------------------------ |
| `core`           | Foundational errors and shared types |

### 2.2 External Dependencies

| Dependency                                     | Purpose                                                |
| ---------------------------------------------- | ------------------------------------------------------ |
| `tokio::fs` or the standard filesystem library | Reading and writing workspace files                    |
| `serde` / `serde_json` / frontmatter parsers   | Parsing structured heartbeat and routine configuration |

### 2.3 Boundary Notes

- `workspace` is not the same as `memory`
- `workspace` does not record message-level Trace or Job data
- `workspace` does not directly schedule heartbeat; it only provides configuration and file contents

---

## 3. Public Interfaces

```rust
pub struct WorkspaceManager {
    pub root: PathBuf,
    pub identity_files: IdentityFiles,
}

pub struct IdentityFiles {
    pub agents: Option<String>,
    pub soul: Option<String>,
    pub user: Option<String>,
    pub identity: Option<String>,
    pub heartbeat: Option<String>,
}

pub struct HeartbeatSpec {
    pub interval: Duration,
    pub routines: Vec<RoutineSpec>,
}

pub struct RoutineSpec {
    pub id: String,
    pub schedule: RoutineSchedule,
    pub prompt: String,
    pub notify_channel: Option<String>,
}
```

Suggested additional interfaces:

```rust
impl WorkspaceManager {
    pub fn load_identity_files(&self) -> Result<IdentityFiles>;
    pub fn load_heartbeat_spec(&self) -> Result<Option<HeartbeatSpec>>;
}
```

---

## 4. Implementation Details

### 4.1 Responsibility Split Across Identity Files

- `AGENTS.md`
  Runtime constraints, roles, and high-level rules
- `SOUL.md`
  Personality, tone, and preferences
- `USER.md`
  Long-term user profile
- `IDENTITY.md`
  System or instance identity description
- `HEARTBEAT.md`
  Long-running plans and recurring tasks

### 4.2 Heartbeat and Routine

`workspace` only defines configuration; it does not execute it directly. The execution chain should be:

```text
workspace/HEARTBEAT.md
    │
    ▼
WorkspaceManager::load_heartbeat_spec()
    │
    ▼
agent::HeartbeatRunner / RoutineScheduler
```

### 4.3 Boundary with memory

The two are easy to confuse, but their responsibilities differ:

- `workspace`
  Persistent identity and strategy files
- `memory`
  Retrievable, recallable, and expirable user memory

Identity file changes usually affect the system prompt; memory changes usually affect recall.

---

## 5. Collaboration with Other Modules

| Module   | Collaboration                                                                              |
| -------- | ------------------------------------------------------------------------------------------ |
| `agent`  | Reads identity files to build the system prompt and load heartbeat / routine configuration |
| `skills` | Provides trusted local skill directories                                                   |
| `trace`  | Identity file version changes should be recorded in provenance                             |
| `memory` | Complements memory without overlapping responsibility                                      |

---

## 6. Implementation Recommendations

- Missing identity files should degrade gracefully and must not block system startup
- Validate heartbeat configuration with schema checks; a bad file should not invalidate the entire workspace
- Identity file changes should carry an explicit version stamp or content hash for Trace recording
