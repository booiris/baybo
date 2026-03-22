# Aura Module Documentation Index

This directory contains module-level design documents organized by Aura crate. Each document aims to cover:

- Module responsibilities and boundaries
- Dependency relationships
- Public interfaces
- Key implementation constraints
- Collaboration with other modules

These documents are meant to be used together with [architecture.md](docs/architecture.md):

- `architecture.md` covers the system-level overview, dependency directions, and key design decisions
- `modules/*.md` cover module-level interfaces, boundaries, and implementation constraints

## Reading Order

It is recommended to read bottom-up along the dependency graph:

1. [core.md](docs/modules/core.md)
2. [channels.md](docs/modules/channels.md)
3. [llm.md](docs/modules/llm.md)
4. [security.md](docs/modules/security.md)
5. [sandbox.md](docs/modules/sandbox.md)
6. [tools.md](docs/modules/tools.md)
7. [registry.md](docs/modules/registry.md)
8. [skills.md](docs/modules/skills.md)
9. [memory.md](docs/modules/memory.md)
10. [workspace.md](docs/modules/workspace.md)
11. [context.md](docs/modules/context.md)
12. [session.md](docs/modules/session.md)
13. [trace.md](docs/modules/trace.md)
14. [job.md](docs/modules/job.md)
15. [cost.md](docs/modules/cost.md)
16. [hook.md](docs/modules/hook.md)
17. [storage.md](docs/modules/storage.md)
18. [agent.md](docs/modules/agent.md)
19. [wasm-runtime.md](docs/modules/wasm-runtime.md)

## Module Groups

### Foundational Types Layer

- [core.md](docs/modules/core.md)
  Shared foundational types. Does not define business traits.

### Ingress and Security Boundary Layer

- [channels.md](docs/modules/channels.md)
  Multi-channel message ingress and delivery.
- [security.md](docs/modules/security.md)
  Input sanitization, secret management, output re-sanitization, and network policy decision interfaces.

### Capability and Governance Layer

- [llm.md](docs/modules/llm.md)
  LLM provider wrapping and response parsing.
- [sandbox.md](docs/modules/sandbox.md)
  Execution isolation layer that uniformly hosts WASM and container sandboxes.
- [tools.md](docs/modules/tools.md)
  Tool abstraction, registration, capability declarations, and runtime routing.
- [registry.md](docs/modules/registry.md)
  Extension registry, artifact verification, and installation governance.
- [skills.md](docs/modules/skills.md)
  Declarative skill definitions, selection, trust tiers, and hot reload.
- [memory.md](docs/modules/memory.md)
  Long-term memory storage and recall.
- [workspace.md](docs/modules/workspace.md)
  Workspace, identity files, heartbeat, and routine configuration.
- [context.md](docs/modules/context.md)
  Context appending, compression, snapshots, and restoration.

### Runtime and Observability Layer

- [session.md](docs/modules/session.md)
  Session lifecycle and session storage interfaces.
- [trace.md](docs/modules/trace.md)
  Call chains, snapshot rollback, and provenance.
- [job.md](docs/modules/job.md)
  Task state machine and state history.
- [cost.md](docs/modules/cost.md)
  Token usage records and spending guards.
- [hook.md](docs/modules/hook.md)
  Lifecycle extension points.

### Infrastructure and Assembly Layer

- [storage.md](docs/modules/storage.md)
  Backend implementations of each Store trait.
- [agent.md](docs/modules/agent.md)
  Assembly layer for Actor, AgentLoop, ToolExecutor, HeartbeatRunner, and ObservabilityRecorder.
- [wasm-runtime.md](docs/modules/wasm-runtime.md)
  Supplemental details for the `WasmRuntime` subcomponent inside `sandbox`.

## Crate Dependency Table

The table below describes the primary direct dependencies to help clarify module boundaries. The implementation should follow [architecture.md](docs/architecture.md) as the source of truth.

| Crate       | Direct Dependencies                                                                                                                  | Notes                                                                      |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| `core`      | No in-workspace dependencies                                                                                                         | Shared foundational types layer                                            |
| `channels`  | `core`                                                                                                                               | Channel ingress and message adaptation                                     |
| `llm`       | `core`                                                                                                                               | LLM provider wrapping                                                      |
| `security`  | `core`                                                                                                                               | Sanitization, secret management, and network policy decision interfaces    |
| `sandbox`   | No hard dependency on business crates                                                                                                | Execution isolation layer, including WASM and container execution surfaces |
| `tools`     | `core`, `sandbox`                                                                                                                    | Tool abstraction, registration, and runtime routing                        |
| `registry`  | `core`                                                                                                                               | Extension artifact discovery, verification, and installation               |
| `skills`    | `core`                                                                                                                               | Declarative skills, selection, and hot reload                              |
| `memory`    | `core`                                                                                                                               | Long-term memory abstraction and management                                |
| `workspace` | `core`                                                                                                                               | Workspace identity files and heartbeat rules                               |
| `context`   | `core`                                                                                                                               | Context window, compression, and snapshots                                 |
| `session`   | `core`                                                                                                                               | Session storage interfaces and management                                  |
| `job`       | `core`                                                                                                                               | Job state machine and task history                                         |
| `trace`     | `core`, `context`                                                                                                                    | Call tracing, snapshot rollback, provenance                                |
| `cost`      | `core`                                                                                                                               | Token usage records and spending guards                                    |
| `hook`      | `core`                                                                                                                               | Lifecycle hook system                                                      |
| `storage`   | `core`, `session`, `memory`, `trace`, `security`, `cost`, `job`                                                                      | Backend implementations of Store traits                                    |
| `agent`     | `core`, `llm`, `tools`, `skills`, `memory`, `workspace`, `context`, `session`, `trace`, `job`, `security`, `cost`, `hook`, `sandbox` | Top-level assembly and runtime execution engine                            |

### Dependency Direction Overview

The overall dependency graph can be approximated as:

```text
core
  ├── channels
  ├── llm
  ├── security
  ├── tools ───► sandbox
  ├── registry
  ├── skills
  ├── memory
  ├── workspace
  ├── context
  ├── session
  ├── job
  ├── cost
  ├── hook
  └── trace ───► context

storage ───► session / memory / trace / security / cost / job
agent   ───► llm / tools / skills / memory / workspace / context / session / trace / job / security / cost / hook / sandbox
```

A few easy-to-confuse points:

- `tools` does not depend on `security`; secrets and network policy are injected by `agent`
- `skills` does not directly depend on `registry` to perform installation, but its governance model consumes source and trust metadata issued by the registry
- `security` is responsible for sanitization and decision interfaces; it does not directly execute containers or open network access
- `sandbox` is responsible for actual execution isolation and network constraints; it does not handle tool registration or skill governance
- `workspace` is not the same as `memory`: the former manages identity files and long-lived strategy, while the latter manages retrievable memory
- `agent` is the assembly layer and should not push lower-level capabilities back into `core`

## Key Constraints

When reading and implementing these modules, the following global constraints are assumed by default:

- Traits are defined in their own modules; `core` only contains shared data types
- Logs, Trace, and Job must not record sensitive plaintext, only placeholders or sanitized summaries
- Tool and skill extensions must carry source, version, hash, trust level, and capability declarations
- High-risk execution must not stay on pure WASM by default; it must be upgraded to the container execution surface in `sandbox`
- The Job state machine is fixed as:

```text
Pending -> InProgress -> Completed -> Submitted -> Accepted
                     \-> Failed
                     \-> Stuck -> InProgress
                              \-> Failed
```

- Multimedia content is passed by reference and must not duplicate raw binary data in sessions, snapshots, or Trace
- Skill hot reload, tool updates, workspace identity file changes, and provider configuration changes should all leave provenance records in Trace
- Heartbeat, routine, cron, and background tool execution must all enter Job and Trace; bypassing the observability chain is not allowed

## Follow-up Recommendation

If the documentation is refined further, keep a consistent template:

1. Module overview
2. Dependencies
3. Public interfaces
4. Implementation details
5. File structure
6. Implementation recommendations
