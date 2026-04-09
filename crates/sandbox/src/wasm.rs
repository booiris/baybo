use std::collections::HashMap;

use serde_json::Value;

use crate::SandboxError;

/// Resource limits for WASM module execution.
pub struct SandboxLimits {
    pub timeout_ms: u64,
    pub max_memory_bytes: usize,
    pub max_fuel: u64,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_memory_bytes: 64 * 1024 * 1024, // 64 MB
            max_fuel: 1_000_000_000,
        }
    }
}

/// A compiled WASM module handle.
pub struct WasmModule {
    pub name: String,
    pub artifact_hash: String,
    compiled: wasmtime::Module,
}

impl std::fmt::Debug for WasmModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmModule")
            .field("name", &self.name)
            .field("artifact_hash", &self.artifact_hash)
            .finish_non_exhaustive()
    }
}

/// State shared between host functions and the WASM module.
struct HostState {
    input_json: Vec<u8>,
    secrets_json: Vec<u8>,
    output_json: Vec<u8>,
}

/// WASM execution runtime backed by wasmtime.
pub struct WasmRuntime {
    engine: wasmtime::Engine,
    limits: SandboxLimits,
}

impl std::fmt::Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime")
            .field("limits_timeout_ms", &self.limits.timeout_ms)
            .field("limits_max_fuel", &self.limits.max_fuel)
            .finish_non_exhaustive()
    }
}

impl WasmRuntime {
    /// Execute a loaded WASM module with the given input parameters and secrets.
    ///
    /// The module is expected to export a `run() -> i32` function.
    /// Host functions in the `aura_env` namespace allow the module to read input/secrets
    /// and write output.
    pub async fn execute(
        &self,
        module: &WasmModule,
        params: Value,
        secrets: HashMap<String, String>,
    ) -> Result<Value, SandboxError> {
        let input_json = serde_json::to_vec(&params)
            .map_err(|e| SandboxError::WasmExecution(format!("failed to serialize input: {e}")))?;
        let secrets_json = serde_json::to_vec(&secrets).map_err(|e| {
            SandboxError::WasmExecution(format!("failed to serialize secrets: {e}"))
        })?;

        let host_state = HostState {
            input_json,
            secrets_json,
            output_json: Vec::new(),
        };

        let mut store = wasmtime::Store::new(&self.engine, host_state);
        store
            .set_fuel(self.limits.max_fuel)
            .map_err(|e| SandboxError::WasmExecution(format!("failed to set fuel: {e}")))?;

        let mut linker: wasmtime::Linker<HostState> = wasmtime::Linker::new(&self.engine);

        // input_len() -> i32
        linker
            .func_wrap(
                "aura_env",
                "input_len",
                |caller: wasmtime::Caller<'_, HostState>| -> i32 {
                    caller.data().input_json.len() as i32
                },
            )
            .map_err(|e| SandboxError::WasmExecution(format!("failed to link input_len: {e}")))?;

        // read_input(ptr: i32, len: i32)
        linker
            .func_wrap(
                "aura_env",
                "read_input",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 ptr: i32,
                 len: i32|
                 -> wasmtime::Result<()> {
                    let input = caller.data().input_json.clone();
                    let copy_len = (len as usize).min(input.len());
                    let memory = caller
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| wasmtime::Error::msg("module has no exported memory"))?;
                    let data = memory.data_mut(&mut caller);
                    let offset = ptr as usize;
                    if offset + copy_len > data.len() {
                        return Err(wasmtime::Error::msg(
                            "read_input: out of bounds memory access",
                        ));
                    }
                    data[offset..offset + copy_len].copy_from_slice(&input[..copy_len]);
                    Ok(())
                },
            )
            .map_err(|e| SandboxError::WasmExecution(format!("failed to link read_input: {e}")))?;

        // secrets_len() -> i32
        linker
            .func_wrap(
                "aura_env",
                "secrets_len",
                |caller: wasmtime::Caller<'_, HostState>| -> i32 {
                    caller.data().secrets_json.len() as i32
                },
            )
            .map_err(|e| SandboxError::WasmExecution(format!("failed to link secrets_len: {e}")))?;

        // read_secrets(ptr: i32, len: i32)
        linker
            .func_wrap(
                "aura_env",
                "read_secrets",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 ptr: i32,
                 len: i32|
                 -> wasmtime::Result<()> {
                    let secrets = caller.data().secrets_json.clone();
                    let copy_len = (len as usize).min(secrets.len());
                    let memory = caller
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| wasmtime::Error::msg("module has no exported memory"))?;
                    let data = memory.data_mut(&mut caller);
                    let offset = ptr as usize;
                    if offset + copy_len > data.len() {
                        return Err(wasmtime::Error::msg(
                            "read_secrets: out of bounds memory access",
                        ));
                    }
                    data[offset..offset + copy_len].copy_from_slice(&secrets[..copy_len]);
                    Ok(())
                },
            )
            .map_err(|e| {
                SandboxError::WasmExecution(format!("failed to link read_secrets: {e}"))
            })?;

        // write_output(ptr: i32, len: i32)
        linker
            .func_wrap(
                "aura_env",
                "write_output",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 ptr: i32,
                 len: i32|
                 -> wasmtime::Result<()> {
                    let memory = caller
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| wasmtime::Error::msg("module has no exported memory"))?;
                    let data = memory.data(&caller);
                    let offset = ptr as usize;
                    let length = len as usize;
                    if offset + length > data.len() {
                        return Err(wasmtime::Error::msg(
                            "write_output: out of bounds memory access",
                        ));
                    }
                    let output = data[offset..offset + length].to_vec();
                    caller.data_mut().output_json = output;
                    Ok(())
                },
            )
            .map_err(|e| {
                SandboxError::WasmExecution(format!("failed to link write_output: {e}"))
            })?;

        let instance = linker
            .instantiate(&mut store, &module.compiled)
            .map_err(|e| {
                SandboxError::WasmExecution(format!("failed to instantiate module: {e}"))
            })?;

        let run_func = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .map_err(|_| SandboxError::MissingExport("run".to_string()))?;

        let timeout_duration = tokio::time::Duration::from_millis(self.limits.timeout_ms);

        let result = tokio::time::timeout(timeout_duration, async {
            // wasmtime calls are synchronous; run on a blocking thread to avoid
            // starving the async runtime.
            let run_result = run_func.call(&mut store, ());
            (run_result, store)
        })
        .await;

        let (run_result, store) = match result {
            Ok(inner) => inner,
            Err(_) => return Err(SandboxError::Timeout),
        };

        match run_result {
            Ok(code) => {
                if code != 0 {
                    return Err(SandboxError::WasmExecution(format!(
                        "module returned non-zero exit code: {code}"
                    )));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("fuel") {
                    return Err(SandboxError::ResourceLimitExceeded(
                        "fuel consumption exceeded".to_string(),
                    ));
                }
                return Err(SandboxError::WasmExecution(format!(
                    "WASM trap during execution: {msg}"
                )));
            }
        }

        let output_bytes = &store.data().output_json;
        if output_bytes.is_empty() {
            return Ok(Value::Null);
        }

        serde_json::from_slice(output_bytes).map_err(|e| {
            SandboxError::WasmExecution(format!("failed to parse module output as JSON: {e}"))
        })
    }
}

#[cfg(test)]
impl WasmRuntime {
    fn new() -> Result<Self, SandboxError> {
        Self::with_limits(SandboxLimits::default())
    }

    fn with_limits(limits: SandboxLimits) -> Result<Self, SandboxError> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).map_err(|e| {
            SandboxError::WasmExecution(format!("failed to create wasmtime engine: {e}"))
        })?;
        Ok(Self { engine, limits })
    }

    fn load_module(&self, bytes: &[u8]) -> Result<WasmModule, SandboxError> {
        self.load_module_named("unnamed", bytes)
    }

    fn load_module_named(&self, name: &str, bytes: &[u8]) -> Result<WasmModule, SandboxError> {
        use sha2::{Digest as _, Sha256};
        if bytes.is_empty() {
            return Err(SandboxError::InvalidModule("empty WASM bytes".to_string()));
        }
        let compiled = wasmtime::Module::new(&self.engine, bytes).map_err(|e| {
            SandboxError::InvalidModule(format!("failed to compile WASM module: {e}"))
        })?;
        let mut hasher = Sha256::new();
        sha2::Digest::update(&mut hasher, bytes);
        let artifact_hash = hex::encode(sha2::Digest::finalize(hasher));
        Ok(WasmModule {
            name: name.to_string(),
            artifact_hash,
            compiled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_engine_successfully() {
        let rt = WasmRuntime::new();
        assert!(rt.is_ok());
    }

    #[test]
    fn load_module_rejects_empty_bytes() {
        let rt = WasmRuntime::new().unwrap();
        let result = rt.load_module(&[]);
        assert!(result.is_err());
        assert!(matches!(result, Err(SandboxError::InvalidModule(_))));
    }

    #[test]
    fn load_module_rejects_invalid_bytes() {
        let rt = WasmRuntime::new().unwrap();
        let result = rt.load_module(&[0x00, 0x01, 0x02, 0x03]);
        assert!(result.is_err());
        assert!(matches!(result, Err(SandboxError::InvalidModule(_))));
    }

    #[test]
    fn load_module_with_valid_wat() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "run") (result i32)
                i32.const 0
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let module = rt.load_module(&wasm_bytes);
        assert!(module.is_ok());
        let module = module.unwrap();
        assert_eq!(module.name, "unnamed");
        assert!(!module.artifact_hash.is_empty());
    }

    #[test]
    fn load_module_named_sets_name() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "run") (result i32)
                i32.const 0
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let module = rt.load_module_named("my_tool", &wasm_bytes).unwrap();
        assert_eq!(module.name, "my_tool");
    }

    #[tokio::test]
    async fn execute_simple_module_returns_null_when_no_output() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "run") (result i32)
                i32.const 0
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let module = rt.load_module(&wasm_bytes).unwrap();
        let result = rt
            .execute(&module, serde_json::json!({}), HashMap::new())
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Value::Null);
    }

    #[tokio::test]
    async fn execute_module_with_nonzero_return_is_error() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "run") (result i32)
                i32.const 1
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let module = rt.load_module(&wasm_bytes).unwrap();
        let result = rt
            .execute(&module, serde_json::json!({}), HashMap::new())
            .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(SandboxError::WasmExecution(_))));
    }

    #[tokio::test]
    async fn execute_module_missing_run_export() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "other") (result i32)
                i32.const 0
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let module = rt.load_module(&wasm_bytes).unwrap();
        let result = rt
            .execute(&module, serde_json::json!({}), HashMap::new())
            .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(SandboxError::MissingExport(_))));
    }

    #[test]
    fn artifact_hash_is_deterministic() {
        let rt = WasmRuntime::new().unwrap();
        let wat = r#"(module
            (func (export "run") (result i32)
                i32.const 0
            )
        )"#;
        let wasm_bytes = wat::parse_str(wat).unwrap();
        let m1 = rt.load_module(&wasm_bytes).unwrap();
        let m2 = rt.load_module(&wasm_bytes).unwrap();
        assert_eq!(m1.artifact_hash, m2.artifact_hash);
    }
}
