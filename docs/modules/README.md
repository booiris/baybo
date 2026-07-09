# Baybo Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [config.md](config.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [cron.md](cron.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md)
4. [tools.md](tools.md) → [workspace.md](workspace.md) → [subagent.md](subagent.md) → [task.md](task.md) → [context.md](context.md)
5. [trace.md](trace.md)
6. [storage.md](storage.md) → [janitor.md](janitor.md) → [pairing.md](pairing.md) → [agent.md](agent.md) → [setup.md](setup.md) → [bootstrap.md](bootstrap.md) → [cli.md](cli.md) → [gateway.md](gateway.md) → [tui.md](tui.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata, MessageSource), governance types (TrustLevel, ArtifactSource, ExtensionManifest), and pure-data persistence types (`CronJob` family, `CostRecord`/`CostSummary`/`TimeRange`). No business traits.
- **store** — `baybo-store`: the persistence **ports** crate. Owns the shared `StorageError` and every `*Store` trait contract (`SessionStore`, `SessionSummaryStore`, `SessionFolderStore`, `TaskStore`, `JobStore`, `TraceStore`, `CostStore`, `SecretStore`, `CronStore`, `BlobStore`, `ChannelPairingStore`, `ChannelSessionStore`, `ChannelBotStore`, `DeviceStore`, `SkillRiskStore`) plus the row/DTO types those traits exchange. Depends only on `model`; `baybo-storage` is the libsql adapter that implements the traits. `JobStore` / `TraceStore` trade in row DTOs (`JobRow` / `JobTransitionRow`, `StepRow` / `SpanRow` / `SpanEventRow` — a queryable key plus the serialized entity in `data`) so the trait can sit here as a leaf while the rich `Job` / `Step` / `Span` types and their state-machine / recorder logic stay in `job` / `trace`, which own the `to_row` / `from_row` conversions.
- **config** — Root `BayboConfig` with JSON loading and `validate()`. Sections (llm, agent, session, channels, security, tools, trace, cost, workspace). Uses mirror structs to stay decoupled from domain crates.

### Ingress and Security Boundary Layer

- **channels** — In-process channel/connection registry, shared channel-domain message types (`IncomingMessage`, `OutgoingMessage`, `AgentEvent`), slash/dashboard trait definitions (`SlashHandler`, `DashboardProvider`, `ViewKind`), and a re-export of the `wire` crate as `baybo_channels::wire`. No built-in adapters — the terminal UI now lives in its own `baybo-tui` crate (see [`tui.md`](./tui.md)).
- **security** — Cryptographic primitives (EncryptionKey, encrypt/decrypt), leak detection (LeakDetector), error types.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing. Subscription/OAuth flavoured providers documented in [`llm-openai-subscription.md`](llm-openai-subscription.md).
- **tools** — Tool abstraction, registration, capability declarations, runtime routing. The `mcp` submodule ships an MCP client (config in `<workspace>/.mcp.json`, OAuth via rmcp) that surfaces every server's tools to the agent loop as `<server>/<tool>`; the `McpReconciler` keeps the registry in sync without a gateway restart.
- **[sandbox](sandbox.md)** — OS-native per-invocation isolation for tools declaring `ToolCapability::ExecCommand`. `bwrap` on Linux, `sandbox-exec` on macOS, `docker` as a cross-platform fallback when the native backend is unavailable; the `ToolExecutor` injects a `SandboxAdapter` into `ToolContext.sandbox` when available. If Baybo detects an outer container/sandbox, Bash silently skips the inner sandbox; if no backend is available on a non-container host, Bash warns before running without it. Filesystem-scoped to the workspace; network gated all-or-nothing on the manifest's `Http` capability.
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **[skills-assessor](skills-assessor.md)** — LLM-backed risk classifier for skills. Hashes the skill directory, caches verdicts (`Safe`/`Suspicious`/`Dangerous`) in `SkillRiskStore`, tiers large skills (primary-scope synchronous + full-scope background worker with restart-safe job recovery), and gates skill injection in `AgentLoop` so only `Dangerous` blocks. Kept separate from `skills` so selection stays deterministic and offline-capable.
- **[subagent](subagent.md)** — Typed subagents: the `SubagentProfile` + process-wide `SubagentRegistry`, the per-root fan-out `SubagentDispatchLimiter`, and the `spawn_subagent` tool. Profiles load from `<workspace>/agents/<name>.md` and the profile's system prompt fully replaces the parent's Soul in the spawned child actor. Like `skills`/`cron`, it owns its own `Tool` and depends on `baybo-tools` for the trait. External `claude`/`codex`/`gemini` backends are documented in [`../external-agents.md`](../external-agents.md).
- **[task](task.md)** — The session planning-checklist tools (`TaskCreate`/`TaskGet`/`TaskList`/`TaskUpdate`), modeled on Claude Code's `Task*` and Codex's `update_plan`. Like `cron`/`skills`/`subagent`, it owns its own `Tool` impls over the `TaskStore` trait. The `Task`/`TaskStatus`/`TaskId` value types live in `model`, the `TaskStore` trait in `store`, the libsql impl over the dedicated `session_tasks` table in `storage`. The agent loop re-injects the list into the model every turn and emits it to the web checklist (`AgentEvent::TaskList` → `Frame::TaskList`). `TaskStop`/`TaskOutput` (the background half) stay stubbed.
- **workspace** — Identity files and long-running configuration.
- **cron** — Cron scheduling: the `CronScheduler` business logic and `CronError`, standard cron syntax. The cron data types (`CronJob`, `CronExecution`, `CronStatus`, `CronSchedule`, `ExecutionStatus`) now live in `model` (re-exported here for back-compat); the `CronStore` trait lives in `store`.
- **context** — Per-actor token budget + compression strategy + transcript ownership (`ContextManager`). Pure in-memory; persistence is the agent loop's job, brokered through `SessionStore` from `session`.
- **session** — Owns the session business logic: the `SessionManager` facade (CRUD, timeout cleanup, transcript / summary persistence helpers) and `SessionError` (with `From<StorageError>`). The `SessionStore` / `SessionSummaryStore` traits and their `StoredMessage` / `SessionSummaryRow` row types now live in `store`; session domain types (`User`, `ChannelType`, `Session`, `SessionState`, `TriggerSource`, `Lineage`) live in `model`; `baybo-storage` provides the libsql implementations. A `Session` is the top of one trace tree (1 trace = 1 session); subagent spawn creates new sessions linked through `Lineage`.

### Runtime and Observability Layer

- **trace** — Trace types + row conversions + lifecycle recorder: `Step` / `Span` / `SpanEvent` domain types and the `SpanRecorder` lifecycle facade with its `TraceEvent` / `TraceEventStream` broadcast bus. The `TraceStore` trait lives in `store` and trades in `StepRow` / `SpanRow` / `SpanEventRow`; this crate owns the `to_row` / `from_row` conversions and converts at the recorder boundary. Half-open-span recovery utility included. Closed strong-typed enums; OTel-aligned naming.
- **job** — Job types + row conversions + state machine + lifecycle orchestrator: `Job` (with `origin`), `JobStatus`, `JobInputKind`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`, and the `JobLifecycle` persistence orchestrator (with `JobCancellationRegistry` and lifecycle-event bus). The `JobStore` trait lives in `store` and trades in `JobRow` / `JobTransitionRow`; this crate owns the state machine and the `to_row` / `from_row` conversions. `Cancelled` and `Failed` are independent terminal states.
- **memory** — Pluggable long-term memory: a single `Memory` trait (one registered `Arc<dyn Memory>`, storage-opaque) with `recall` (sync) + `on_job_complete`/`on_session_end` (background) hooks + `tools()`. Core ships the trait + a `NoopMemory` default; no real backend yet (runtime wires `None`, an inert no-op path). The agent loop drives recall/write for `UserChat`+`Cron` jobs; recalled memories inject as framed `MessageSource::RecalledMemory` rows, never `Role::System`. See `docs/modules/memory.md`.
- **cost** — LLM-call spend tracking: the `CostManager` (synchronous in-memory accumulator + async persist + `LlmCallGuard` bridge via `cost_call_guard`) and `CostError`. The `CostStore` trait lives in `store`; data types (`CostRecord`, `CostSummary`, `TimeRange`) live in `model`. Integer `MicroUsd` arithmetic — never floats.
- **query** — Read-only analytics surface (`QueryApi`) over Session / Job / Step / Span / Cost. One `QueryError` collapses four upstream store error types; CLI and gateway admin handlers use it without re-deriving error shape.

### Infrastructure and Assembly Layer

- **storage** — The libsql **adapter**: implements every `*Store` trait from `store` over a single libsql backend and bundles the impls in `Store` for DI. All trait contracts + their row types (incl. `RiskVerdict` / `RiskLevel`, `ChannelPairingRow` / `PairingStatus`, `JobRow`, `StepRow` / `SpanRow` / `SpanEventRow`) now live in `store`; consumers import them from `baybo_store` directly (`baybo-storage` does **not** re-export them — it exposes only the `Store` DI bundle, the `libsql` module, and the `retry` helper). Normal deps are just `store` + `model` — no longer on any domain crate (`session` / `job` / `trace` / `cost` / `cron` / `memory` / `security`). `job` / `trace` remain only as `dev-dependencies`, so the round-trip tests can build the rich types.
- **[janitor](janitor.md)** — `baybo-janitor`. Best-effort, cadence-driven maintenance outside the agent loop: filesystem TTL sweeps (rotated log files at 30d, stale sidecar-cache dirs at 7d) plus the hourly `channel_pairings` retention purge (pending-expired + approved older than 7d). No storage compaction. Spawned by the gateway boot path.
- **[pairing](pairing.md)** — Per-user pairing gate for sidecar-routed inbound messages. `PairingService` checks the `(channel_type, bot_id, user_id)` triple, mints 6-char codes for unknown senders, and refuses with a `Frame::Notice` until `baybo pair approve <code>` flips the row to `approved`. The `ChannelPairingStore` trait + its row type live in `store` (imported directly from `baybo_store`); `baybo-pairing` is the service + code generator.
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, cost management (`CostManager` as a `TraceEventStream` subscriber, with `CostGuardError` for limit breaches), and `SecurityGateway` (cross-cutting interception facade tied to the execution path). Observability facades live in their domain crates now: `JobLifecycle` in `baybo-job`, `SpanRecorder` in `baybo-trace`, the `Memory` trait in `baybo-memory`, `SessionManager` in `baybo-session`. Agent assembles them via dependency injection. Bridges cron domain types and storage row types.
- **[setup](setup.md)** — Interactive first-run wizard (`baybo-setup`, exposed as `baybo setup`). Bootstraps the workspace skeleton, mints the master encryption key under `<root>/.key/encryption.key`, writes a default `baybo.json`, opens libsql + the secret vault, then runs Quick / Full step sequences (LLM / channel / browser). Same flow primitives back `baybo llm add` / `baybo channel add` (`flow::configure_*_step`), so the wizard's per-step UX is structurally identical to the argv path. β2 commit semantics — `baybo.json` is the only deferred write.
- **bootstrap** — Binary entry point (`crates/baybo/src/main.rs`) and `boot` submodule. Loads `BayboConfig`, translates each section into domain types, and wires the Arc graph that `agent` consumes. Unit-tested mappings live in `boot`; Arc lifetime management stays in `main.rs`.
- **cli** — Operator-facing command layer (`baybo-cli`). One `clap` tree drives both argv-mode commands (`baybo config show`) and in-conversation slash commands (`/config show`). Read-only and mutating commands share a single dispatcher; slash input that resolves to a CLI command never enters the agent's context. User-invocable skills are the one sanctioned exception: `/<skill>` is forwarded to the agent as a normal chat message so `SkillRegistry::select` can narrow on the exact-match branch.
- **[tui](tui.md)** — Interactive terminal UI (`baybo-tui`). Ratatui + Crossterm frontend driven by a WS+MessagePack `WsTransport` client of `baybo-gateway`; no local manager graph, no workspace singleton. Hosts `TuiAdapter`, `TuiSlashHandler`, and `TuiDashboardProvider`. Input-history persistence is delivered over the same WS via `Frame::HistorySnapshot` / `Frame::HistoryAppend` — the TUI never opens the vault itself. Depends on `baybo-channels` for shared trait definitions only.
- **[gateway](gateway.md)** — Headless HTTP backend (`baybo-gateway`). One axum server is both a `ChannelType::Http` adapter (chat flows through the normal Router path) and an admin REST/SSE API mirroring the CLI families. Auth is a dynamic per-install token stored in `SecretVault`; platform service units live behind `linux` / `macos` Cargo features (one knob per OS; reuse these for any future platform-specific gateway code). Driven by the `baybo gateway …` command tree.

## Feature Subsystems (cross-crate)

- [agent-profiles.md](agent-profiles.md) — User-managed chat personas (`AgentProfile`): DB-backed profiles bundling name, description, avatar (blob), system prompt (`NULL` = Soul), framework (`baybo`/`claude`/`codex`), and an optional LLM pin, with a locked built-in `baybo` row. Skills are shown read-only live from the registry, not stored on the profile. Spans `model` (id + framework types), `store`/`storage` (`AgentProfileStore` + `agent_profiles` table), `gateway` (`/v1/agents` CRUD), and the web **Agents** page. v1 is management-only — no runtime consumer reads profiles yet; distinct from `subagent`'s `SubagentProfile`.
- [mobile/companion.md](mobile/companion.md) — The iOS companion app (`app/mobile`): scan-to-connect pairing + end-to-end-encrypted remote notifications. Spans `device-proto` (XXpsk0 pairing + Noise IK content + AEAD previews), `pairing` (`DevicePairingService`), `gateway` (the A-side host leg, content responder, relay-content manager, push dispatcher), and the separate `remote-host/` workspace (C — blind relay + APNs). 1:1 binding, the content/pairing relay path, push pipeline, and the cross-workspace e2e harness.
- [mobile/pairing-security.md](mobile/pairing-security.md) — The pairing **threat model** and crypto design: why device pairing is safe against a hostile relay. The `rendezvous_id` / 256-bit-`secret` split, `Noise_XXpsk0`, the high-entropy-secret invariant, prologue binding, confirm-code channel binding, and secret hygiene.
- [mobile/relay-push-security.md](mobile/relay-push-security.md) — The mobile scan-to-pair, relay, and push security note: QR bootstrap, Noise IK content legs, encrypted APNs previews, remote-host transparency, proof sketches, and explicit security boundaries.
- [mobile/blob-transfer.md](mobile/blob-transfer.md) — Dedicated relay blob legs for mobile attachment download/upload, including chat-priority bandwidth, token-gated `BlobStore::open_at`, upload limits, and the mobile Rust/Tauri/TS client.

## Cross-Cutting Guides

- [testing.md](../testing.md) — Test pyramid (unit / crate-level / cross-crate), `test-support` gating, fixture inventory, and the six conventions every new test should follow.
- [background-compression.md](../background-compression.md) — Async per-session summary maintenance; runs as an in-actor detached step on the parent's `AgentLoop`, attributing to the parent session. Trigger gates in `agent`, fast-path strategy + summary metadata in `context`, on-disk summary file under `<workspace>/state/sessions/<id>/`, `session_summaries` table in `storage`.

## Dependency Overview

```
model (owns Session/User/ChannelType/SessionState + message types; no internal deps)
  ├── channels ──► model
  ├── llm ──► model
  ├── context ──► model
  ├── security ──► model, channels
  ├── trace ──► model, context, job
  ├── tools ──► model
  ├── cron ──► model
  ├── skills ──► model
  ├── skills-assessor ──► skills, storage, llm, model
  └── job (no internal deps)
  └── workspace (no internal deps)
  └── config (no internal deps; external only)

store     ──► model (ports crate: every *Store trait contract + the row/DTO types they exchange + StorageError; no logic)
storage   ──► store, model (implements every *Store trait from the ports crate; exposes the Store DI bundle + libsql + retry; sole backend: libsql)
janitor   ──► store, workspace, model (best-effort background sweeps; consumes ChannelPairingStore; spawned by the gateway)
context   ──► model, llm, skills, session, subagent, tools, trace (owns ContextManager; pure in-memory; resolves subagent_type→system prompt; persistence routed via SessionStore)
session   ──► model, store (owns SessionManager + SessionError; the SessionStore / SessionSummaryStore traits live in store)
pairing   ──► model, store (owns PairingService + code generator; consumes ChannelPairingStore + ChannelPairingRow + PairingStatus from the ports crate)
sandbox   ──► (no internal deps; OS sandbox runner consumed by agent)
subagent  ──► model, session, tools (typed subagents + spawn_subagent tool; owns its Tool like skills/cron; profiles from <workspace>/agents/)
task      ──► model, store, tools (planning-checklist Task* tools over the TaskStore trait; owns its Tool like cron/skills/subagent)
agent     ──► model, llm, tools, subagent, skills, skills-assessor, workspace, context, session, trace, job, memory, cost, cron, security, sandbox, storage, channels
gateway   ──► agent, channels, config, context, cron, cost, job, llm, model, pairing, query, security, session, skills, storage, store, tools, trace, workspace
tui       ──► channels, model, tools (trait defs + shared types; talks to gateway over HTTP+SSE)
setup     ──► channels, config, gateway, llm, model, security, storage, workspace (interactive first-run wizard; baybo-cli's llm-add/channel-add wrap its flow primitives)
bootstrap ──► config + all domain crates it assembles (entry point only)
```

## Key Constraints

- Each module defines its own error type; no shared error enum
- Store trait contracts live in `store`; `storage` is the libsql adapter; domain types live in their own crates; business logic stays out of the storage layer
- Logs, Trace, and Job must not record sensitive plaintext — only placeholders or sanitized summaries
- Tool/skill extensions must carry source, version, hash, trust level, and capability declarations
- The Job state machine is fixed: `Pending → InProgress → Completed` (with `Stuck`, `Failed`, and `Cancelled` branches). `Cancelled` carries a `reason` and `partial_artifacts: Vec<SpanId>` for resume context
- Multimedia passed by reference — no raw binary in sessions or Trace
- Hot reload, tool updates, identity changes, and config changes must leave provenance records in Trace
- Cron and background execution must all enter Job and Trace
