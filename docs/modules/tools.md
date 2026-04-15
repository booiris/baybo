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

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly. This keeps `AgentLoop` independent of execution shape.

### Secret declaration and runtime access

Tools declare their secret requirements via `secret_requirements() -> Vec<SecretRequirement>`. Each requirement specifies a key, an access level (`ReadOnly` or `ReadWrite`), and whether the secret is required or optional.

At execution time, `agent::ToolExecutor` injects secrets into `ToolContext` in two ways:
- A static `secrets: HashMap<String, SecretValue>` snapshot
- A `secret_accessor: Option<Arc<dyn SecretAccessor>>` for runtime get/set with permission enforcement

The `SecretAccessor` trait is defined in the tools crate but implemented as `ScopedSecretAccessor` in the agent crate, enforcing that tools can only access declared keys and can only write keys declared with `ReadWrite` access. There is no direct dependency on `security`.

### MCP client support (removed — pending reintroduction)

MCP client support previously lived here (`McpTool`, `McpToolProvider`, stdio
and HTTP transports via the `rmcp` SDK, `{server_name}/{tool_name}`
namespacing, trust/capability inheritance from `McpServerConfig`). It was
removed pending the final tool-system design. The re-add plan lives in
`docs/todo/reintroduce-mcp-support.md`.

### Capability-driven governance

`ToolManifest` carries hard constraints (`ToolCapability`): `ReadWorkspace`, `WriteWorkspace`, `Http(domains)`, `SpawnProcess`, `BrowserAutomation`. Before execution, manifest capabilities are merged with governance ceilings from trust level.

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools should forbid `WriteWorkspace` and `SpawnProcess` by default
- Undeclared HTTP domains are always denied

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. `secret_requirements`, capabilities, and trust level are never exposed.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

## Constraints

- Depends on `model`, `session`, `registry` (the `rmcp` dep returns with MCP support)
- Does not install third-party artifacts (that's `registry`)
- Does not approve network/filesystem permissions — only consumes already-decided policies
- `artifact_hash` must be recorded in `trace::ExecutionProvenance`

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `ToolExecutor` executes tools, injects secrets via `ScopedSecretAccessor`, records observability |
| `security` | Upper layers inject secrets and network policy (no direct dependency) |
| `registry` | Provides verified third-party tool artifacts; `TrustLevel` will govern MCP tools once reintroduced |
| `trace` | Records tool parameters, results, artifact hash, and source |
| `llm` | Consumes tool definitions for function calling |
| `rmcp` | (Removed) External SDK for MCP client transports — to be restored with MCP support |
