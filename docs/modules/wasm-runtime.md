# wasm-runtime - WASM Sub-Runtime Inside sandbox

## Overview

This document describes the `WasmRuntime` subcomponent inside `sandbox`. For system-level isolation design, see [sandbox.md](sandbox.md).

`WasmRuntime` is responsible for:

- Loading WASM bytecode and compiling into executable modules
- Instantiating under a restricted host ABI
- Injecting parameters and the minimum required secrets
- Controlling fuel, memory, and timeout

It is not the full isolation governance layer. High-risk tasks must be upgraded to the container execution surface.

## Design Decisions

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

- Always put a timeout on `execute()` — do not rely on tools to exit voluntarily
- Use precompiled caches to reduce repeated load overhead
- If a tool requires capabilities beyond WASM, return an explicit "requires container" error

## Collaboration

| Module | Role |
|--------|------|
| `sandbox` | `WasmRuntime` is the default execution subcomponent |
| `tools` | `WasmTool` holds `Arc<WasmRuntime>` and calls `execute()` |
| `security` | Upper layers pass already-decrypted, minimized secrets |
| `trace` | Artifact hash and execution result recorded in provenance |
| `agent` | `ToolExecutor` wraps the WASM tool execution lifecycle |
