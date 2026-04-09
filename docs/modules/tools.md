# tools - Tool System

## Overview

The `tools` crate provides Aura's tool abstraction, registration, and runtime routing. It exposes a uniform `Tool` interface so Agent does not care whether a tool is a built-in Rust implementation, a WASM tool, or a high-risk tool that runs in a container.

Core responsibilities:

- Define `Tool` trait and `ToolOutput`
- Manage `ToolRegistry` for built-in and WASM tools
- Generate tool definitions for the LLM (name, description, parameters schema only — no secrets or governance details)
- Carry source, capability, trust, and runtime metadata in `ToolManifest`
- Route execution requests to `WasmRuntime` or container sandbox via `sandbox`

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly, WASM tools are adapted by `WasmTool`, and high-risk tools are forwarded to `sandbox` at runtime. This keeps `AgentLoop` independent of execution shape.

### Tool does not fetch secrets

`tools` declares requirements via `required_secrets()` only. Secrets, network policy, and sandbox policy are injected into `ToolContext` by `agent::ToolExecutor` before execution. There is no direct dependency on `security`.

### Capability-driven governance

`ToolManifest` carries hard constraints (`ToolCapability`): `ReadWorkspace`, `WriteWorkspace`, `Http(domains)`, `SpawnProcess`, `BrowserAutomation`. Before execution, three inputs are merged: manifest capabilities, governance ceilings from trust level, and admin-configured sandbox/network policy.

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools should forbid `WriteWorkspace` and `SpawnProcess` by default
- `BrowserAutomation` or `SpawnProcess` triggers container escalation
- Undeclared HTTP domains are always denied

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. `required_secrets`, capabilities, trust level, and sandbox policy are never exposed.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

## Constraints

- Depends on `core` and `sandbox`
- Does not install third-party artifacts (that's `registry`)
- Does not approve network/filesystem permissions — only consumes already-decided policies
- `artifact_hash` must be recorded in `trace::ExecutionProvenance`

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `ToolExecutor` executes tools, injects secrets, records observability |
| `security` | Upper layers inject secrets and network policy (no direct dependency) |
| `sandbox` | Provides WASM and container execution |
| `registry` | Provides verified third-party tool artifacts |
| `trace` | Records tool parameters, results, artifact hash, and source |
| `llm` | Consumes tool definitions for function calling |
