# wasm-runtime - The WASM Sub-Runtime Inside sandbox

## 1. Module Positioning

This document describes only the `WasmRuntime` subcomponent inside the `sandbox` crate. For system-level execution isolation design, use [sandbox.md](docs/modules/sandbox.md) as the primary reference.

`WasmRuntime` is responsible for:

- Loading WASM bytecode and compiling it into executable modules
- Instantiating modules under a restricted host ABI
- Injecting parameters and the minimum required secrets
- Controlling fuel, memory, and timeout

It is not the full isolation governance layer. High-risk tasks must be upgraded to the container execution surface.

---

## 2. Public Interfaces

```rust
pub struct WasmRuntime {
    engine: wasmtime::Engine,
}

impl WasmRuntime {
    pub fn load_module(&self, wasm_bytes: &[u8]) -> Result<WasmModule>;

    pub async fn execute(
        &self,
        module: &WasmModule,
        params: Value,
        secrets: HashMap<String, SecretValue>,
    ) -> Result<Value>;
}
```

Suggested additional types:

```rust
pub struct WasmModule {
    pub name: String,
    pub artifact_hash: String,
    compiled: wasmtime::Module,
}

pub struct SandboxLimits {
    pub timeout_ms: u64,
    pub max_memory_bytes: usize,
    pub max_fuel: u64,
}
```

---

## 3. Implementation Details

### 3.1 Module Loading Flow

```text
load_module(wasm_bytes)
    │
    ├── validate bytecode format
    ├── compute artifact hash
    ├── wasmtime::Module::new(engine, bytes)
    └── return WasmModule
```

### 3.2 Execution Flow

```text
execute(module, params, secrets)
    │
    ├── create Store / Linker / Instance
    ├── inject controlled host functions
    ├── inject serialized params and secrets
    ├── set fuel / memory / timeout
    ├── call the agreed export function (such as run)
    └── parse the returned JSON Value
```

### 3.3 Sandbox Boundary

By default, the WASM sub-runtime must not have:

- Arbitrary filesystem read/write access
- Arbitrary network access
- Permission to read host environment variables
- Arbitrary host function call capability

If higher privileges are required, upgrade policy at the `sandbox` layer instead of quietly loosening restrictions inside `WasmRuntime`.

### 3.4 Secret Injection Strategy

The `secrets` passed to `execute()` should contain only the minimum set declared by the current tool. Internally, the runtime should:

- Expose those secrets only to the current instance
- Never write secrets into logs
- Release the corresponding memory objects promptly after execution

### 3.5 Error Model

It is recommended to distinguish among:

- Module load failure
- Missing export function
- Parameter deserialization failure
- Execution timeout
- Fuel or memory limit exceeded
- Tool-level execution error

```rust
pub enum WasmRuntimeError {
    InvalidModule(String),
    MissingExport(String),
    Timeout,
    ResourceLimitExceeded(String),
    Execution(String),
}
```

---

## 4. Collaboration with Other Modules

| Module     | Collaboration                                                              |
| ---------- | -------------------------------------------------------------------------- |
| `sandbox`  | Invoked as the default execution subcomponent                              |
| `tools`    | `WasmTool` holds `Arc<WasmRuntime>` and calls `execute()`                  |
| `security` | Upper layers pass already decrypted and minimized secrets in as parameters |
| `trace`    | Artifact hash and execution result are written into provenance and result  |
| `agent`    | `ToolExecutor` wraps the WASM tool execution lifecycle                     |

---

## 5. Implementation Recommendations

- Use precompiled caches to reduce repeated load overhead
- Always put a timeout on `execute()`; do not rely on tools to exit voluntarily
- Cover malicious cases in tests, such as infinite loops, oversized memory allocations, and invalid return values
- If a tool requires capabilities beyond what WASM can support, return an explicit "requires container execution surface" error rather than failing implicitly
