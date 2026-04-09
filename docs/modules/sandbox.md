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

## Constraints

- Does not strongly depend on other business crates
- Every execution path must have timeouts and resource limits
- Container image, policy, and allowlist summaries must be recorded for auditability

## Collaboration

| Module | Role |
|--------|------|
| `tools` | Provides tool manifests and execution requests |
| `agent` | Selects execution policy and records observability |
| `security` | Provides network admission decisions |
| `trace` | Records runtime shape, artifact hash, and failure reasons |

See [wasm-runtime.md](wasm-runtime.md) for detailed `WasmRuntime` subcomponent design.
