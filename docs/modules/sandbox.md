# sandbox - Execution Isolation Layer

## Overview

The `sandbox` crate is Aura's execution isolation layer, extending tool execution into a layered model:

- Use `WasmRuntime` by default
- Upgrade high-risk operations to a container sandbox
- Route all external network access through policy checks

It owns actual execution privileges. It does not handle tool registration, skill governance, or secret custody.

## Design Decisions

### Layered execution model

The execution surface is decided by combining tool manifest capabilities, trust level, and admin config:

- Pure computation or restricted HTTP → WASM
- Declares `SpawnProcess` or `BrowserAutomation` → container
- Declares `WriteWorkspace` without sufficient trust → reject

Four policy tiers: `WasmOnly` (default), `WorkspaceWrite` (workspace read/write), `ContainerRestricted` (container with proxy-controlled network), `ContainerElevated` (trusted-only with admin approval + full audit).

### Deny-by-default networking

Network access is denied by default. Only domains declared in the manifest and approved by policy may be accessed. Both container and WASM paths share the same allowlist logic. Loopback access must be explicitly configured.

### Secret injection boundary

Sandbox only consumes already-decrypted, short-lived secrets. It does not read from persistent storage, write secrets into logs/Trace/container snapshots, and releases memory objects immediately after execution.

### Why containers exist

The container sandbox covers tasks pure WASM cannot handle: browser automation, shell/package managers, long-running external programs, and tasks requiring stronger filesystem isolation. It is not the default path — enabled only when both manifest and governance allow it.

## WasmRuntime Subcomponent

`WasmRuntime` is the default execution engine inside `sandbox`, responsible for:

- Loading WASM bytecode and compiling into executable modules
- Instantiating under a restricted host ABI
- Injecting parameters and the minimum required secrets
- Controlling fuel, memory, and timeout

High-risk tasks must be upgraded to the container execution surface.

### Module loading flow

Validate bytecode format → compute artifact hash → compile via wasmtime → return `WasmModule`.

### Execution flow

Create Store/Linker/Instance → inject controlled host functions → inject serialized params and secrets → set fuel/memory/timeout → call the agreed export function (e.g. `run`) → parse returned JSON.

### Sandbox boundary

By default, the WASM sub-runtime must **not** have: arbitrary filesystem access, arbitrary network access, host environment variable reading, or arbitrary host function calls. If higher privileges are needed, escalate at the `sandbox` layer — never quietly loosen restrictions inside `WasmRuntime`.

### Secret injection

Secrets passed to `execute()` contain only the minimum set declared by the current tool. The runtime exposes them only to the current instance, never writes them into logs, and releases memory promptly after execution.

### Error model

Distinguish among: module load failure, missing export function, parameter deserialization failure, execution timeout, fuel/memory limit exceeded, and tool-level execution error.

## Constraints

- Does not strongly depend on other business crates
- Every execution path must have timeouts and resource limits
- Container image, policy, and allowlist summaries must be recorded for auditability
- Always put a timeout on `execute()` — do not rely on tools to exit voluntarily
- Use precompiled caches to reduce repeated load overhead
- If a tool requires capabilities beyond WASM, return an explicit "requires container" error

## Collaboration

| Module | Role |
|--------|------|
| `tools` | Provides tool manifests and execution requests; `WasmTool` holds `Arc<WasmRuntime>` and calls `execute()` |
| `agent` | Selects execution policy, wraps WASM tool execution lifecycle, and records observability |
| `security` | Provides network admission decisions; upper layers pass already-decrypted, minimized secrets |
| `trace` | Records runtime shape, artifact hash, execution result, and failure reasons |
