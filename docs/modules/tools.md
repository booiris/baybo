# tools - Tool System

## Overview

The `tools` crate provides Aura's tool abstraction, registration, and runtime routing. It exposes a uniform `Tool` interface so Agent does not care how a particular tool is implemented.

Core responsibilities:

- Define `Tool` trait and `ToolOutput`
- Manage `ToolRegistry` — both built-in tools (registered at startup) and **dynamic** tools sourced from external providers like MCP servers (registered/unregistered at runtime via `register_dynamic` / `unregister_for_source`)
- Generate tool definitions for the LLM (name, description, parameters schema only — no secrets or governance details)
- Carry source, capability, trust, and runtime metadata in `ToolManifest`

### Builtin tool set

Modeled after Claude Code's
[tools reference](https://code.claude.com/docs/en/tools-reference). Tool
names match the strings the LLM uses in function calls and operators use in
permission rules.

| Tool                                                                                                                                                                                                                                                                  | Status      | Notes                                                                                                       |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ----------------------------------------------------------------------------------------------------------- |
| `Read`, `Write`, `Edit`                                                                                                                                                                                                                                               | implemented | file I/O on absolute paths                                                                                  |
| `Bash`                                                                                                                                                                                                                                                                | implemented | `sh -c`; no env/cwd persistence across calls                                                                |
| `Glob`, `Grep`                                                                                                                                                                                                                                                        | implemented | basic walkdir + regex; will be upgraded if throughput becomes an issue                                      |
| `WebFetch`                                                                                                                                                                                                                                                            | implemented | returns raw body; no side-channel LLM extraction yet                                                        |
| `Echo`                                                                                                                                                                                                                                                                | debug-only  | returns params verbatim; registered only under `debug_assertions` for round-trip smoke-testing              |
| `CronCreate`, `CronDelete`, `CronList`                                                                                                                                                                                                                                | implemented | exported from `aura_cron::agent_tools` (not `aura-tools::builtin`) because they hold `Arc<CronScheduler>`; registered from `src/runtime.rs` after the scheduler is constructed |
| `Agent`, `AskUserQuestion`, `SendMessage`, `EnterPlanMode`/`ExitPlanMode`, `EnterWorktree`/`ExitWorktree`, `LSP`, `Monitor`, `NotebookEdit`, `Skill`, `Task*`/`TodoWrite`, `ToolSearch`, `WebSearch`, `Team*`                                                          | TODO stub   | lives in `builtin::todo`; not auto-registered — each depends on a backing subsystem that has not yet landed |

`ToolRegistry::with_defaults()` registers the implemented set with
`TrustLevel::Trusted` manifests declaring their capabilities
(`ReadFile`, `WriteFile`, `Http`, `ExecCommand`). Stubs exist so downstream
can register them once their backing subsystem is ready without having to
invent the tool name/schema at that point.

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly. This keeps `AgentLoop` independent of execution shape.

### Secret access (deferred)

Tool-level secret declaration and runtime injection were removed pending the
final tool-system design. `ToolContext` currently carries no secrets; a future
iteration will reintroduce per-tool secret access on top of the finalized
`Tool` trait and governance model.

### MCP client support

The `mcp` submodule (`crates/tools/src/mcp/`) implements an MCP **client**
that surfaces every tool advertised by a configured MCP server through the
agent loop's `Tool` path. Per the workspace's "MCP scope is agent-loop only"
rule, MCP tools never bridge to slash, mention, or elicitation surfaces.

- **Configuration** lives in `<workspace>/.mcp.json` (loaded/written by
  `aura_tools::mcp::McpFile`). Each entry carries a `name`, a transport
  (`stdio { command, args }` or `http { url }`), a `trust_level`, an
  optional `capabilities` set, and an optional `oauth { client_id,
  callback_port }` block. **Nothing secret lives in this file** — env
  bags, header bags, OAuth client secrets, and OAuth refresh/access
  tokens all live in `SecretVault` under the `mcp.<name>.…` namespace
  (`aura_tools::mcp::vault_keys`).
- **Tool wrapping** — every server-side tool descriptor becomes an
  `aura_tools::mcp::McpTool` named `<server>/<tool>` so MCP names cannot
  collide with builtins. Each `McpTool` carries an `Arc`-cloned
  `Peer<RoleClient>` that proxies `call_tool` over the connected
  rmcp transport.
- **Reconciler** (`McpReconciler`) re-reads `.mcp.json` every 5 seconds,
  computes a per-entry identity hash (transport + trust + capabilities +
  OAuth client_id), and connects/disconnects accordingly. Connections
  are torn down + re-established when the identity hash changes;
  `register_dynamic` / `unregister_for_source` keep the registry in
  sync. Cancelled via the shared shutdown signal.
- **OAuth** — the `oauth` submodule (`aura_tools::mcp::oauth`) drives
  OAuth 2.1 + PKCE + Dynamic Client Registration via rmcp's
  `OAuthState`. The flow runs **inline inside `aura mcp add`** for HTTP
  transports when any OAuth flag (`--client-id`, `--client-secret`,
  `--callback-port`) is passed: discovery → DCR (if no client_id) →
  PKCE → browser launch via `open::that` → localhost callback listener
  (axum) on `--callback-port` (or an ephemeral port) → token exchange →
  vault persistence. Failed authorization → no `.mcp.json` mutation.
- **Trust + capabilities** — the entry's `trust_level` becomes the
  `ToolManifest`'s ceiling; defaults are `[Http]` for HTTP and
  `[Http, ExecCommand]` for stdio. The existing
  `ToolExecutor::validate_trust` rule still fires (e.g. an `installed`
  server may not declare `WriteFile` or `ExecCommand`). Each `McpTool`
  reports a single `ResourceAccess::Http { host }` (HTTP) or
  `ResourceAccess::ExecCommand { command }` (stdio) so the approval
  gate can prompt per host or per command.

### Capability-driven governance

`ToolManifest` carries coarse capability ceilings (`ToolCapability`): `ReadFile`, `WriteFile`, `Http`, `ExecCommand`. The manifest answers "what *kind* of thing may this tool do"; the concrete resource per call comes from `Tool::accessed_resources(params)` as [`ResourceAccess`] and is what the approval gate routes on. Trust level is a separate axis enforced before execution.

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools may not declare `WriteFile` or `ExecCommand` (requires `Trusted`)
- Concrete paths/hosts/commands are gated by user approval, not by manifest

### User-approval gate

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. At execution time it calls `gate_map.get(user.channel)` to resolve the right gate for the session's channel; if no gate is registered, `AutoDenyGate` (fail-closed) is returned. Matching:

- `ReadFile` / `WriteFile` — component-aware path prefix (`Path::starts_with`). Approving `/tmp/a` covers `/tmp/a/b` but not `/tmp/ab`. Read and write are independent (an approved read does not cover a write).
- `Http` — `HostPattern::Exact` is case-insensitive equality; `HostPattern::Wildcard("foo.com")` covers `foo.com` and any subdomain but not `barfoo.com`. `ResourceAccess::to_approved()` produces `Exact` only — wildcards are operator-authored.
- `ExecCommand` — exact full-command string match (no shell tokenization).

`ApprovalDecision::ApproveAlways` promotes every `ResourceAccess` the call touched into `ApprovedResource` entries that the executor pushes directly into the shared `Mutex<Vec<ApprovedResource>>` provided by `AgentLoop`. After all tool calls in a turn complete, `AgentLoop` flushes the contents back into `SessionState::approved_resources` so they survive session replay.

`ChannelApprovalGate` + `ApprovalQueue` (`crates/tools/src/approval.rs`) extract the common queue-and-oneshot pattern so each channel only supplies a sync waker callback (e.g. `|| event_tx.try_send(WakeUp)`). The queue exposes `peek_head` / `resolve_head` / `len` so the channel's event loop can render and dismiss inline prompts without touching oneshot internals.

`ApprovalGateMap` is a sync `HashMap<ChannelType, Arc<dyn ApprovalGate>>` behind a `std::sync::RwLock`. `ChannelRegistry` populates it at `register()` time by reading `Channel::approval_gate()` on the newly-registered handle and evicts the entry on `unregister()`; `ToolExecutor` reads it per-call. Both hold an `Arc` to the same map, so gates registered after `ToolExecutor` construction are visible immediately. Adding a new channel with approval support requires only wiring an `Arc<dyn ApprovalGate>` into the `Channel` at construction time — no changes to `ToolExecutor` or bootstrap code.

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. Capabilities and trust level are never exposed.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

## Constraints

- Depends on `model`, `session`, `registry`, plus `rmcp` + `oauth2` + `axum` (callback listener) for the MCP client
- Does not install third-party artifacts (that's `registry`)
- Defines the `ApprovalGate` trait but never implements the user-facing UX — the per-connection gate is built by the gateway's WS sidecar (`ChannelApprovalGate` backed by an `ApprovalQueue`), and the TUI renders the resulting prompts inline in its scrollback
- `artifact_hash` must be recorded in `trace::ExecutionProvenance`

## Collaboration

| Module     | Role                                                                                               |
| ---------- | -------------------------------------------------------------------------------------------------- |
| `agent`    | `ToolExecutor` validates trust/capability, executes tools, records observability                   |
| `security` | Upper layers inject secrets and network policy (no direct dependency)                              |
| `registry` | Provides verified third-party tool artifacts; `TrustLevel` will govern MCP tools once reintroduced |
| `trace`    | Records tool parameters, results, artifact hash, and source                                        |
| `llm`      | Consumes tool definitions for function calling                                                     |
| `rmcp`     | (Removed) External SDK for MCP client transports — to be restored with MCP support                 |
