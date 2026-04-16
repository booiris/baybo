# tools - Tool System

> **Status note.** MCP client support (`McpTool`, `McpToolProvider`, `rmcp`
> transports, the `tools.mcp_servers[]` config surface) has been temporarily
> removed pending the final shape of the tool system. Plan to re-add is
> tracked in `docs/todo/reintroduce-mcp-support.md`. Sections below that refer
> to MCP describe the target state once it lands again.

## Overview

The `tools` crate provides Aura's tool abstraction, registration, and runtime routing. It exposes a uniform `Tool` interface so Agent does not care how a particular tool is implemented.

Core responsibilities:

- Define `Tool` trait and `ToolOutput`
- Manage `ToolRegistry` for built-in tools (MCP tools will rejoin once support is reintroduced)
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
| `Agent`, `AskUserQuestion`, `SendMessage`, `Cron*`, `EnterPlanMode`/`ExitPlanMode`, `EnterWorktree`/`ExitWorktree`, `LSP`, `Monitor`, `NotebookEdit`, `Skill`, `Task*`/`TodoWrite`, `ToolSearch`, `WebSearch`, `Team*`, `ListMcpResourcesTool`, `ReadMcpResourceTool` | TODO stub   | lives in `builtin::todo`; not auto-registered — each depends on a backing subsystem that has not yet landed |

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

### MCP client support (removed — pending reintroduction)

MCP client support previously lived here (`McpTool`, `McpToolProvider`, stdio
and HTTP transports via the `rmcp` SDK, `{server_name}/{tool_name}`
namespacing, trust/capability inheritance from `McpServerConfig`). It was
removed pending the final tool-system design. The re-add plan lives in
`docs/todo/reintroduce-mcp-support.md`.

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

`ApprovalGateMap` is a sync `HashMap<ChannelType, Arc<dyn ApprovalGate>>` behind a `std::sync::RwLock`. `ChannelRegistry` populates it at `register()` time by calling `ChannelAdapter::approval_gate()`; `ToolExecutor` reads it per-call. Both hold an `Arc` to the same map, so gates registered after `ToolExecutor` construction are visible immediately. Adding a new channel with approval support requires only implementing `fn approval_gate() -> Option<Arc<dyn ApprovalGate>>` on the adapter — no changes to `ToolExecutor` or bootstrap code.

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. Capabilities and trust level are never exposed.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

## Constraints

- Depends on `model`, `session`, `registry` (the `rmcp` dep returns with MCP support)
- Does not install third-party artifacts (that's `registry`)
- Defines the `ApprovalGate` trait but never implements the user-facing UX — that lives in `channels` (`TuiApprovalGate`)
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
