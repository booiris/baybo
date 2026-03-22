# Aura Development Guide

**Aura** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Build & Test

```bash
cargo fmt                                                      # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                     # unit tests
RUST_LOG=aura=debug cargo run                                  # run with logging
```

## Code Style

- Prefer `crate::` for cross-module imports; `super::` is fine in tests and intra-module refs
- No `pub use` re-exports unless exposing to downstream consumers
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- Use `thiserror` for error types in `error.rs`
- Map errors with context: `.map_err(|e| SomeError::Variant { reason: e.to_string() })?`
- Prefer strong types over strings (enums, newtypes); use typed structs instead of `HashMap<String, Value>`, only keep an `extra` field for truly dynamic extensions
- Keep functions focused, extract helpers when logic is reused
- Comments for non-obvious logic only

## Architecture

Prefer generic/extensible architectures over hardcoding specific integrations. Ask clarifying questions about the desired abstraction level before implementing.

**Core design principles**:

- **Modular**: Each crate is an independent module; traits are defined within their own crate; crates interact via traits — high cohesion, low coupling
- **Extensible**: Channels, Tools, and Skills all plug in via registries; Tool/extensions loaded through WASM runtime
- **Secure**: Encrypted secret storage, input leak detection, layered execution isolation (WASM by default, container sandbox as fallback), least-privilege networking and credential injection
- **Governable**: All Skill/Tool/extensions must carry source, version, hash, trust level, and capability declarations; selection and execution are auditable
- **Observable**: Full call-chain tracing; Job system manages all async operation states; supports session replay, trace forking and rollback; logs/traces record only sanitized placeholders and summaries
- **Reliable**: Built-in error recovery, retry, and degradation strategies
- **Actor model**: Message events decoupled from execution via Actor-based concurrency
- **Long-running**: Supports heartbeat, background routines, workspace identity files, and daemon-style operation

All I/O is async with tokio. Use `Arc<T>` for shared state, `RwLock` for concurrent access.

### Trait Ownership

Each crate defines its own traits and exposes them as interaction contracts. `core` only contains the most fundamental shared types (Message, User, Session data structures, OperationKind, common error types) — **no business traits**.

Key traits for extensibility:

| Crate      | Trait / Key Type                                                                        |
| ---------- | --------------------------------------------------------------------------------------- |
| `channels` | `ChannelAdapter`                                                                        |
| `llm`      | `LlmProviderFactory`, `LlmProviderRegistry`                                             |
| `tools`    | `Tool`, `ToolRegistry`                                                                  |
| `skills`   | `SkillDefinition`, `SkillRegistry`                                                      |
| `memory`   | `MemoryStore`, `MemoryManager`                                                          |
| `context`  | `ContextManager`, `Tokenizer`                                                           |
| `session`  | `SessionStore`, `SessionManager`                                                        |
| `trace`    | `TraceStore`, `TraceCollector`                                                          |
| `security` | `SecretStore`, `SecretVault`, `LeakDetector`                                            |
| `cost`     | `CostStore`, `CostTracker`, `CostGuard`                                                 |
| `hook`     | `Hook`, `HookManager`                                                                   |
| `job`      | `JobStore`, `JobManager`, `JobStatus`                                                   |
| `agent`    | `AgentActor`, `AgentSupervisor`, `Router` (assembly layer, depends on all above traits) |
| `storage`  | Implements Store traits from each crate (in-memory / SQLite)                            |

### Agent Internal Responsibilities

- **AgentActor**: Message dispatch and Actor lifecycle only
- **AgentLoop**: Core conversation loop (LLM call → parse → Tool/Skill dispatch)
- **ToolExecutor**: Tool execution + Job/Trace recording
- **ObservabilityRecorder**: Unified wrapper for Job + Trace + Cost recording logic
- **HeartbeatRunner / RoutineScheduler**: Long-running and proactive task scheduling

## Module Design Specs

**Before working on any crate, always read its corresponding design document in `docs/modules/` first.** The design doc is the source of truth for that module's architecture, trait definitions, and implementation details. Code should follow the spec; the spec is the tiebreaker when in doubt.

Overall architecture: `docs/architecture.md` · Module index: `docs/modules/README.md`

## Project Structure

```
aura/
├── Cargo.toml                    # workspace
├── crates/
│   ├── core/                     # Shared base types (no business traits)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── message.rs        # Message, ContentBlock, ChatMessage
│   │       ├── session.rs        # Session
│   │       ├── user.rs           # User, ChannelType
│   │       ├── operation.rs      # OperationKind (shared by Job/Trace)
│   │       └── error.rs          # AuraError
│   │
│   ├── channels/                 # ChannelAdapter trait + implementations
│   │   └── src/
│   │       ├── lib.rs            # ChannelAdapter trait
│   │       ├── http.rs
│   │       ├── telegram.rs
│   │       ├── discord.rs
│   │       └── cli.rs
│   │
│   ├── llm/                      # LLM Client (built on rig)
│   │   └── src/
│   │       ├── lib.rs            # LlmClient, ModelInfo, LlmResponse
│   │       ├── registry.rs       # LlmProviderRegistry + LlmProviderFactory trait
│   │       ├── providers/        # Built-in providers (openai, anthropic, ollama)
│   │       ├── rig_adapter.rs    # Tool → rig Tool adapter
│   │       └── prompt_guided.rs  # Non-function-calling response parsing
│   │
│   ├── tools/                    # Tool trait + ToolRegistry + WASM tools
│   │   └── src/
│   │       ├── lib.rs            # Tool trait, ToolContext, ToolOutput
│   │       ├── registry.rs       # ToolRegistry
│   │       └── wasm.rs           # WasmTool, ToolManifest
│   │
│   ├── registry/                 # Extension registry, signature verification, install governance
│   │   └── src/
│   │       ├── lib.rs            # ExtensionManifest, RegistryInstaller
│   │       ├── catalog.rs        # Registry index loading
│   │       └── verifier.rs       # Hash/signature verification
│   │
│   ├── skills/                   # SkillDefinition + SkillRegistry + hot-reload
│   │   └── src/
│   │       ├── lib.rs            # SkillDefinition, SkillTrigger
│   │       ├── registry.rs       # SkillRegistry
│   │       └── loader.rs         # File loading + hot-watch
│   │
│   ├── memory/                   # MemoryStore trait + MemoryManager
│   │   └── src/
│   │       ├── lib.rs            # MemoryStore trait, MemoryEntry
│   │       └── manager.rs        # MemoryManager
│   │
│   ├── workspace/                # Workspace, identity files, heartbeat rules
│   │   └── src/
│   │       ├── lib.rs            # WorkspaceManager, IdentityFiles
│   │       ├── identity.rs       # AGENTS/SOUL/USER/IDENTITY file loading
│   │       └── heartbeat.rs      # Heartbeat config and parsing
│   │
│   ├── context/                  # ContextManager trait + compression strategies
│   │   └── src/
│   │       ├── lib.rs            # ContextManager trait, Tokenizer trait
│   │       ├── sliding_window.rs
│   │       ├── summarize.rs
│   │       └── hybrid.rs
│   │
│   ├── session/                  # SessionStore trait + SessionManager
│   │   └── src/
│   │       ├── lib.rs            # SessionStore trait
│   │       └── manager.rs        # SessionManager
│   │
│   ├── job/                      # Job management system
│   │   └── src/
│   │       ├── lib.rs            # Job, JobStatus, JobStore trait (uses core::OperationKind)
│   │       └── manager.rs        # JobManager
│   │
│   ├── trace/                    # Call-chain tracing
│   │   └── src/
│   │       ├── lib.rs            # TraceStore trait, SessionTrace, TraceNode
│   │       ├── collector.rs      # TraceCollector
│   │       ├── tree.rs           # Trace tree structure
│   │       ├── fork.rs           # Fork / rollback
│   │       └── snapshot.rs       # Context snapshots
│   │
│   ├── security/                 # SecretStore trait + SecurityGateway
│   │   └── src/
│   │       ├── lib.rs            # SecretStore trait
│   │       ├── gateway.rs        # SecurityGateway
│   │       ├── leak_detector.rs  # LeakDetector
│   │       ├── vault.rs          # SecretVault
│   │       └── crypto.rs         # Encryption / decryption
│   │
│   ├── cost/                     # CostStore trait + CostTracker
│   │   └── src/
│   │       ├── lib.rs            # CostStore trait, CostRecord
│   │       ├── tracker.rs        # CostTracker
│   │       └── guard.rs          # CostGuard
│   │
│   ├── hook/                     # Hook trait + HookManager
│   │   └── src/
│   │       ├── lib.rs            # Hook trait, HookPoint, HookAction
│   │       └── manager.rs        # HookManager
│   │
│   ├── agent/                    # Assembly layer: AgentActor, Router, Soul, Cron, ErrorHandler
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── actor.rs          # AgentActor (message dispatch + Actor lifecycle only)
│   │       ├── agent_loop.rs     # AgentLoop (core conversation loop)
│   │       ├── tool_executor.rs  # ToolExecutor (tool execution + observability recording)
│   │       ├── observability.rs  # ObservabilityRecorder (unified Job/Trace/Cost)
│   │       ├── supervisor.rs     # AgentSupervisor
│   │       ├── router.rs         # Router
│   │       ├── soul.rs           # Soul personality system
│   │       ├── cron.rs           # CronScheduler
│   │       ├── heartbeat.rs      # HeartbeatRunner, RoutineScheduler
│   │       ├── error_recovery.rs # ErrorHandler
│   │       └── policy.rs         # ExecutionPolicy
│   │
│   ├── sandbox/                  # Execution isolation (WASM + container sandbox + network policy)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── wasm.rs           # WasmRuntime
│   │       ├── container.rs      # ContainerSandbox
│   │       └── network.rs        # NetworkPolicy / proxy
│   │
│   └── storage/                  # All Store trait implementations
│       └── src/
│           ├── lib.rs            # StorageFactory, StorageSet
│           ├── memory_backend/   # In-memory implementation (dev/test)
│           └── sqlite/           # SQLite implementation (one struct per Store trait)
│               ├── mod.rs
│               ├── session.rs    # SqliteSessionStore
│               ├── memory.rs     # SqliteMemoryStore
│               ├── trace.rs      # SqliteTraceStore
│               ├── secret.rs     # SqliteSecretStore
│               ├── cost.rs       # SqliteCostStore
│               └── job.rs        # SqliteJobStore
│
├── src/
│   └── main.rs                   # Application entry point
│
├── config/
│   ├── default.json
│   └── soul.json
│
├── skills/                       # Skill definition files
│
└── tools/                        # WASM tool directory
```

## Debugging

```bash
RUST_LOG=aura=trace cargo run                # verbose
RUST_LOG=aura::agent=debug cargo run         # agent module only
```
