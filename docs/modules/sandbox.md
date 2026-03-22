# sandbox - Execution Isolation Layer

## 1. Module Overview

The `sandbox` crate is Aura's execution isolation layer. It extends tool execution from a plain WASM runtime into a layered isolation model:

- Use `WasmRuntime` by default
- Upgrade high-risk operations to a container sandbox
- Route all external network access through policy checks and proxy control

It owns actual execution privileges. It does not handle tool registration, skill governance, or secret custody.

---

## 2. Dependencies

### 2.1 Internal Dependencies

`sandbox` does not strongly depend on other business crates. It only exposes generic execution capabilities.

### 2.2 External Dependencies

| Dependency                   | Purpose                                        |
| ---------------------------- | ---------------------------------------------- |
| `wasmtime`                   | WASM module compilation and execution          |
| `tokio`                      | Async execution, timeouts, and process control |
| `serde_json`                 | Parameter and value exchange                   |
| Container runtime SDK or CLI | Starting container sandboxes                   |

### 2.3 Collaboration Boundaries

- `tools` provides manifests and execution requests
- `security` provides network policy decision interfaces
- `agent` chooses which `SandboxPolicy` applies for a given execution

---

## 3. Public Interfaces

### 3.1 SandboxPolicy

```rust
pub enum SandboxPolicy {
    WasmOnly,
    WorkspaceWrite,
    ContainerRestricted,
    ContainerElevated,
}
```

Semantics:

- `WasmOnly`
  Default policy. Allows only controlled host functions and restricted network access.
- `WorkspaceWrite`
  Allows reads and writes within the workspace, but still forbids arbitrary subprocesses and undeclared network access.
- `ContainerRestricted`
  Executes inside a container, with network controlled by proxy and allowlist rules.
- `ContainerElevated`
  Available only to `Trusted` extensions and requires explicit administrator approval plus full audit records.

### 3.2 WasmRuntime

```rust
pub struct WasmRuntime {
    engine: wasmtime::Engine,
}

impl WasmRuntime {
    pub fn load_module(&self, wasm_bytes: &[u8]) -> Result<WasmModule>;
    pub async fn execute(&self, module: &WasmModule, params: Value, secrets: HashMap<String, SecretValue>) -> Result<Value>;
}
```

### 3.3 ContainerSandbox

```rust
pub struct ContainerSandbox {
    image: String,
    network_proxy: Arc<NetworkProxy>,
}

impl ContainerSandbox {
    pub async fn execute(&self, job: ContainerJob, policy: SandboxPolicy) -> Result<ContainerResult>;
}
```

### 3.4 Network Policy Types

```rust
pub struct NetworkPolicy {
    pub allowed_domains: Vec<String>,
    pub allow_loopback: bool,
}

pub struct NetworkProxy {
    policy: NetworkPolicy,
}
```

---

## 4. Implementation Details

### 4.1 Layered Execution Model

Recommended decision flow:

```text
ToolManifest.capabilities + trust_level + admin config
    │
    ▼
agent selects SandboxPolicy
    │
    ▼
sandbox executes via WasmRuntime or ContainerSandbox
```

Typical escalation conditions:

- Declares `SpawnProcess` -> container
- Declares `BrowserAutomation` -> container
- Declares `WriteWorkspace` without sufficient trust -> reject
- Pure computation or restricted HTTP only -> WASM

### 4.2 Deny-by-Default Networking

Network access rules:

- Deny by default
- Only domains declared in the manifest and approved by policy may be accessed
- Both container and WASM paths should go through the same allowlist logic
- Whether loopback is allowed must be configured explicitly

### 4.3 Secret Injection Boundary

`sandbox` only consumes already decrypted, short-lived secrets:

- It does not read secrets from persistent storage
- It does not write secrets into logs, Trace, or container environment snapshots
- It releases the corresponding memory objects immediately after execution

### 4.4 Why the Container Execution Surface Exists

The container sandbox exists for tasks that pure WASM is not suited for:

- Browser automation
- Shell and package managers
- Long-running external programs
- Tasks requiring stronger filesystem isolation

This is not the default path. It is enabled only when both the manifest and governance allow it.

---

## 5. Collaboration with Other Modules

| Module     | Collaboration                                                |
| ---------- | ------------------------------------------------------------ |
| `tools`    | Provides tool manifests, modules, and execution requests     |
| `agent`    | Selects execution policy and records observability uniformly |
| `security` | Provides network admission decision interfaces               |
| `trace`    | Records runtime shape, artifact hash, and failure reasons    |

---

## 6. Implementation Recommendations

- Stabilize `WasmOnly` and `ContainerRestricted` first before opening higher-privilege tiers
- Every execution path must have timeouts and resource limits
- Record container image, policy, and allowlist summaries for auditability
- Let [wasm-runtime.md](docs/modules/wasm-runtime.md) hold the detailed design of the `WasmRuntime` subcomponent
