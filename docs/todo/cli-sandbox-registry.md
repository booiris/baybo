# CLI `sandbox list/info` — Sandbox Registry

## Problem

`docs/modules/cli.md` §"Command surface" lists `sandbox list` and `sandbox info` as deferred. They have no CLI grammar today because there is no subsystem to back them: `SandboxPolicy` is a bare 4-variant enum (`crates/sandbox/src/lib.rs:11-20`) with no runtime registry, no config-driven listing, and no metadata (capability declarations, trust level, resource limits) attached to the policies themselves.

Until there is a thing to enumerate, `list` would return the same static rows regardless of deployment and `info <policy>` would have no fields to print beyond the variant name. Shipping the CLI in that state is worse than deferring — it implies the sandbox subsystem is more introspectable than it is.

## What `list/info` should ultimately look like

- **`sandbox list`** — one row per sandbox policy available in this deployment: name, kind (`wasm`, `container`, …), active/inactive, short description. Operator view: the same policies that `ToolExecutor` can actually select from at runtime, not a hardcoded variant set.
- **`sandbox info <name>`** — full policy metadata: capability declarations (network, filesystem, env), resource limits (memory, cpu, wall-clock), trust level, where the policy came from (config file path, workspace default, downloaded artifact), hash.

## Why it's blocked

1. **No registry.** `SandboxPolicy` is defined as an enum at `crates/sandbox/src/lib.rs:11-20`:
   ```rust
   pub enum SandboxPolicy {
       WasmOnly,
       WorkspaceWrite,
       ContainerRestricted,
       ContainerElevated,
   }
   ```
   Nothing else in the crate materializes a collection of these with attached metadata. There is no `SandboxRegistry`, no config section listing available policies, and no per-policy metadata type.

2. **No enforcement wiring.** `ToolExecutor` (crates/tools/src) does not yet consume a sandbox policy per tool invocation — the execution path is still "run in-process" for most tools. Without enforcement, a registry would be describing something the runtime does not yet honor.

3. **No config surface.** `AuraConfig` has no `sandbox` section; policies are not named, declared, or versioned from config. A `list` command needs something config-driven so that a deployment's actual policy set is what surfaces.

These three gaps are related: a registry is only useful if enforcement reads from it, and enforcement is only useful if config can declare policies by name. They should land together.

## Proposed direction

Ship in three stages; the CLI lands last.

### Stage 1 — policy metadata type

In `aura-sandbox`, introduce `SandboxPolicySpec`:

```rust
pub struct SandboxPolicySpec {
    pub name: String,                         // stable id (config-addressable)
    pub kind: SandboxKind,                    // enum: Wasm, Container, …
    pub capabilities: CapabilityDeclaration,  // network/fs/env allowances
    pub limits: ResourceLimits,               // memory, cpu, wall-clock
    pub trust_level: TrustLevel,              // reuse aura_registry::TrustLevel
    pub source: PolicySource,                 // enum: Config { path }, Workspace, Artifact { hash }
}
```

Split `SandboxPolicy` (the enum used at sites that pick a policy) from `SandboxPolicySpec` (the metadata returned by the registry). This avoids breaking existing match sites while giving the registry something to describe.

### Stage 2 — `SandboxRegistry`

A read-mostly in-memory registry populated at startup from a new `config.sandbox` section:

```rust
pub struct SandboxRegistry { /* BTreeMap<String, SandboxPolicySpec> */ }

impl SandboxRegistry {
    pub fn list(&self) -> Vec<&SandboxPolicySpec>;
    pub fn get(&self, name: &str) -> Option<&SandboxPolicySpec>;
}
```

Threaded into `CommandContext` as `Option<Arc<SandboxRegistry>>` (mirrors how other registries are wired).

### Stage 3 — CLI

Once the registry exists:

- Add `Commands::Sandbox { cmd: SandboxCmd }` with `List` and `Info { name }` variants.
- Handler at `crates/cli/src/commands/sandbox.rs` reads from the registry; purely read-only, no slash-mode gating needed.
- Parser + dispatch smoke tests using an in-memory registry seeded with two specs.
- Update `docs/modules/cli.md` §"Command surface" sandbox row from "deferred" to "shipped".

## Design constraints

- **Never log secret material** — if any sandbox policy ever gains auth tokens (e.g., container registry creds), those must be redacted before `sandbox info` can print. Follow the same placeholder-summary rule the rest of the observability stack uses.
- **Registry must reflect enforcement truth**. If `ToolExecutor` ignores a policy at runtime, `sandbox list` printing it is a lie. Either gate the CLI ship on Stage 2 *and* an enforcement audit, or prefix every row with an enforcement-status flag (`active`, `inactive-no-enforcement`).
- **Config shape is the commitment point**. Once `config.sandbox` is the source of truth, the schema is effectively a public contract (operators edit it). Coordinate with `docs/todo/config-wire-remaining-sections.md` so the sandbox section lands alongside whatever other missing sections are being wired.

## Related

- `docs/modules/cli.md` §"Command surface" row for `sandbox` — deferred note
- `crates/sandbox/src/lib.rs:11-20` — today's bare enum
- `crates/tools/src` — `ToolExecutor`, the eventual consumer of policies
- `docs/todo/config-wire-remaining-sections.md` — config integration
- `docs/todo/archives/cli-write-commands.md` — archived parent todo; this one carries the design work item 12 punted on
