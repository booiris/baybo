# Skill Tool Trust-Level Gating

## Problem

Skills can declare their own tools, but we currently surface all registered
tools to the LLM without considering trust levels. The previous approach
filtered tools via a per-skill `allowed_tools` allowlist (union across active
skills), but this was too coarse — it starved the agent of builtin tools
whenever any skill was active and didn't account for differing trust levels
between builtin and skill-provided tools.

The `allowed_tools` filtering was removed. All registered tools now reach the
LLM unconditionally. This is correct for builtin tools (which are trusted), but
once third-party or user-authored skills start registering their own tools, we
need a trust-level gate so that:

1. Untrusted skill tools require explicit user approval before the LLM can
   invoke them.
2. The approval decision is informed by the tool's `TrustLevel` and the
   skill's provenance (built-in vs. community vs. user-local).
3. Builtin tools remain unconditionally available regardless of active skills.

## Proposed Direction

- When building `tool_defs` in `AgentLoop::call_llm`, annotate each definition
  with its `TrustLevel` from `ToolManifest`.
- Introduce a policy check before sending tool definitions to the LLM:
  tools below a configurable trust threshold are either omitted or sent with a
  flag that triggers the approval gate on execution (the latter is preferred so
  the LLM knows the tool exists but the user can still deny it).
- The approval gate in `ToolExecutor` already checks `accessed_resources`; extend
  it to also consider `TrustLevel` from the manifest, so low-trust tools always
  prompt for approval regardless of resource declarations.
- Skill manifests should propagate their provenance to any tools they register,
  so the trust decision can factor in where the skill came from.

## Open Questions

- Should low-trust tools be hidden from the LLM entirely, or visible but
  gated at execution time? Hiding reduces noise; gating preserves
  discoverability.
- How does trust level interact with the existing `ToolCapability` declarations in
  `ToolManifest`? A high-trust tool with broad capabilities may still warrant
  per-invocation approval.
- Should users be able to promote a skill's tools to a higher trust level via
  configuration?

## Related

- `crates/tools/src/lib.rs` — `ToolManifest`, `ToolCapability`
- `crates/model/src/governance.rs` — `TrustLevel`, `ArtifactSource`
- `crates/agent/src/runtime/tool_executor.rs` — approval gate
- `crates/agent/src/runtime/agent_loop.rs` — tool definition injection in `call_llm`
- `docs/modules/tools.md` — tool system design spec
