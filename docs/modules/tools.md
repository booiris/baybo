# tools - Tool System

## 1. Module Overview

The `tools` crate is responsible for Aura's tool abstraction, tool registration, and runtime routing. It exposes a uniform `Tool` interface upward so Agent does not need to care whether a tool comes from:

- A built-in Rust implementation
- A WASM tool
- A high-risk tool that must be upgraded to the container execution surface

Core responsibilities:

- Define `Tool` and `ToolOutput`
- Manage `ToolRegistry`
- Generate tool definitions for the LLM
- Carry source, capability, trust, and runtime metadata in the tool manifest
- Route tool execution requests to the `WasmRuntime` or container sandbox provided by `sandbox`

`tools` does not depend on `security` directly. Secrets, network policy, and sandbox policy are injected into `ToolContext` by `agent::ToolExecutor` before execution.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose                                                             |
| ---------------- | ------------------------------------------------------------------- |
| `core`           | Shared types such as `User` and `AuraError`                         |
| `sandbox`        | `WasmRuntime`, `SandboxPolicy`, and the container execution surface |

### 2.2 External Dependencies

| Dependency             | Purpose                              |
| ---------------------- | ------------------------------------ |
| `serde` / `serde_json` | Parameters, outputs, and JSON Schema |
| `async-trait`          | Async interface for `Tool`           |
| `tokio`                | Timeouts and cancellation control    |

### 2.3 Boundary Notes

- `tools` does not fetch secrets
- `tools` does not install third-party artifacts; installation belongs to `registry`
- `tools` does not approve network or filesystem permissions; it only consumes policies already decided by upper layers

---

## 3. Public Interfaces

### 3.1 Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn required_secrets(&self) -> Vec<String> { vec![] }
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

### 3.2 ToolContext

```rust
pub struct ToolContext {
    pub session_id: String,
    pub user: User,
    pub timeout: Duration,
    pub cancellation_token: CancellationToken,
    pub secrets: HashMap<String, SecretValue>,
    pub sandbox_policy: SandboxPolicy,
    pub network_policy: NetworkPolicy,
}
```

Constraints:

- `secrets` contains only the minimum set declared by the current tool
- `sandbox_policy` is decided jointly by the governance and execution layers; the tool itself cannot escalate privileges
- `network_policy` contains only approved target domains; undeclared targets must always be denied
- `ToolContext` does not carry Trace or Job recording responsibility

### 3.3 ToolOutput

```rust
pub enum ToolOutput {
    Text(String),
    Json(Value),
    Error(String),
    LargeText { content: String, truncated: bool },
}
```

### 3.4 ToolRegistry

```rust
pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    wasm_tools: HashMap<String, WasmTool>,
    wasm_runtime: Arc<WasmRuntime>,
}

impl ToolRegistry {
    pub fn register_builtin(&mut self, tool: impl Tool + 'static);
    pub fn register_wasm(&mut self, wasm_path: &Path) -> Result<()>;
    pub fn load_wasm_tools_from_dir(&mut self, dir: &Path) -> Result<()>;
    pub fn tool_definitions(&self) -> Vec<ToolDefinition>;
    pub fn get(&self, name: &str) -> Option<&dyn Tool>;
    pub async fn execute(&self, name: &str, params: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

### 3.5 WasmTool

```rust
pub struct WasmTool {
    manifest: ToolManifest,
    module: WasmModule,
    runtime: Arc<WasmRuntime>,
}
```

### 3.6 ToolManifest

```rust
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    pub version: String,
    pub artifact_hash: String,
    pub source: ArtifactSource,
    pub trust_level: TrustLevel,
    pub parameters_schema: Value,
    pub required_secrets: Vec<String>,
    pub capabilities: Vec<ToolCapability>,
    pub preferred_runtime: ToolRuntimeProfile,
}

pub enum ToolRuntimeProfile {
    Wasm,
    Container,
}

pub enum ToolCapability {
    ReadWorkspace,
    WriteWorkspace,
    Http(Vec<String>),
    SpawnProcess,
    BrowserAutomation,
}
```

Constraints:

- `artifact_hash` must be written into `trace::ExecutionProvenance.tool_artifact_hash`
- `source` and `trust_level` are used by governance to decide whether auto-execution is allowed
- `capabilities` are hard constraints, not descriptive text
- `preferred_runtime` is only a preference; whether execution is upgraded to a container is decided by `agent + sandbox`

---

## 4. Implementation Details

### 4.1 Unified Abstraction for Built-in and Isolated Execution Surfaces

`ToolRegistry` exposes a single `Tool` interface upward:

- Rust tools: directly implement `Tool`
- WASM tools: adapted by `WasmTool` to implement `Tool`
- High-risk tools: still exposed through `Tool`, but forwarded by `ToolExecutor` to `sandbox` at runtime

This keeps `AgentLoop` independent of the actual execution shape.

### 4.2 Secret Injection Flow

Recommended flow:

```text
tool.required_secrets()
    │
    ▼
SecretVault::get_secrets_for_tool(...)
    │
    ▼
ToolContext { secrets, sandbox_policy, network_policy, ... }
    │
    ▼
tool.execute(params, &ctx)
```

`tools` declares requirements only. It does not fetch secrets.

### 4.3 Merging Capabilities and Policy

Before actual execution, three kinds of information must be combined:

1. `ToolManifest.capabilities`
2. Governance ceilings implied by `trust_level`
3. `SandboxPolicy` and `NetworkPolicy` from administrators or deployment configuration

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools should forbid `WriteWorkspace` and `SpawnProcess` by default
- Tools declaring `BrowserAutomation` or `SpawnProcess` should be escalated to the container execution surface by default
- Domains not declared in `Http(Vec<String>)` must not be accessible

### 4.4 Tool Loading

Typical flow:

1. Read `manifest.json`
2. Validate required fields, hash, and capability declarations
3. Read `.wasm` bytecode
4. Call `WasmRuntime::load_module()`
5. Construct `WasmTool`
6. Register it in `ToolRegistry`

### 4.5 Output and Context Control

Before tool output enters Agent, it should satisfy:

- Prefer `Json` for structured data
- Use `LargeText` for long text
- Truncate promptly if limits are exceeded
- Sanitize all output before it enters Job or Trace

### 4.6 LLM Visibility

`tool_definitions()` converts tool information into schema visible to the LLM:

- `name`
- `description`
- `parameters_schema`

The following must not be exposed to the model:

- `required_secrets`
- Full capability details
- `trust_level`
- Underlying `sandbox_policy`

---

## 5. Collaboration with Other Modules

| Module     | Collaboration                                                                 |
| ---------- | ----------------------------------------------------------------------------- |
| `agent`    | `ToolExecutor` executes tools, injects secrets, and records observability     |
| `security` | Upper layers inject secrets and network policy; there is no direct dependency |
| `sandbox`  | Provides WASM and container execution                                         |
| `registry` | Provides verified third-party tool artifacts                                  |
| `trace`    | Records tool parameters, results, artifact hash, and source                   |
| `llm`      | Consumes tool definitions for function calling                                |

---

## 6. Implementation Recommendations

- Validate manifests inside `register_wasm()` and fail immediately if required fields are missing
- `ToolOutput::Error` is model-readable, but it must still be sanitized before entering logs
- Treat `ToolCapability` as a hard constraint, not advisory metadata
- High-risk capabilities must trigger explicit runtime escalation rather than silently staying on pure WASM
- Keep execution-isolation details documented separately in [sandbox.md](docs/modules/sandbox.md)
