# skills - Skill System

## Overview

The `skills` crate defines, loads, selects, and hot-reloads declarative skills with governance. It is not just "reading JSON templates" — it also carries governance responsibilities.

**Tool = atomic operation + isolated execution. Skill = declarative orchestration + governance constraints.**

Core responsibilities:

- Skill definition with source, version, and trust level
- Trigger matching (command, regex pattern, agent decision)
- Selection pipeline with scoring, token budget, and tool ceiling
- Constrain which tools a skill may call

## Design Decisions

### Three-tier trust model

- **Trusted**: workspace or admin-placed skills. May hot-reload and request full tool set.
- **Installed**: registry-installed skills. May auto-match but tool count and capabilities are downgraded.
- **Untrusted**: may only be listed and reviewed, cannot auto-execute.

### Selection pipeline

Order: `gating → scoring → token budget → tool ceiling attenuation → final selection`

- **Gating**: filter out skills whose requirements (binaries, env vars, models) are not satisfied
- **Scoring**: rank by command match, regex match, description similarity
- **Token budget**: ensure total injected skill-description tokens stay within budget
- **Tool ceiling attenuation**: lower tool privileges and priority by trust level

### Hot reload constraints

- Watch only trusted directories
- Validate schema and requirements before accepting changes
- Record name/version/source/hash on version replacement
- On failure, keep the old version rather than emptying the registry

### Boundary with tool governance

Skills declare `allowed_tools`, but this is only one input to the upper bound. Before execution, the system still checks: skill's allowlist → trust-level ceiling → `ToolManifest.capabilities` → `sandbox` policy. The skill's allowlist is not the final execution authorization.

## Constraints

- Depends only on `core`
- Does not call `llm` or execute tools directly
- Does not install extensions (that's `registry`)
- Every skill execution must record `skill_name`, `skill_version`, `source`, `trust_level` in Trace

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `SkillRegistry.select()` and executes skills |
| `tools` | Skills declare allowed tool sets but don't execute tools directly |
| `registry` | Supplies source, version, and hash metadata for installed skills |
| `trace` | Records skill version, source, and execution results |
| `workspace` | Provides trusted local skill directories for hot reload |
