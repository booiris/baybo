# skills - Skill System

## 1. Module Overview

The `skills` crate is responsible for defining, loading, selecting, and hot-reloading declarative skills in Aura. Compared with earlier versions, this module is no longer just "reading JSON templates"; it also carries governance responsibilities:

- Describe the source, version, and trust level of a skill
- Perform pre-execution requirements and gating checks
- Select skills by match score, token budget, and tool ceiling
- Constrain which tools a skill may call, instead of relying only on prompt self-discipline

In short: **Tool = atomic operation + isolated execution, Skill = declarative orchestration + governance constraints.**

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Shared types such as `Message` and `AuraError` |

### 2.2 External Dependencies

| Crate | Purpose |
|-------|------|
| `serde` + `serde_json` | Serialization and deserialization of skill definition files |
| `regex` | Regex matching for `SkillTrigger::Pattern` |
| `notify` | Filesystem watching for hot reload |
| `handlebars` or `tera` | Template rendering |

### 2.3 Boundary Notes

- `skills` does not call `llm` directly
- `skills` does not execute tools directly
- `skills` does not install third-party extensions; installation belongs to `registry`
- `skills` is responsible for governance and selection, not final runtime approval

---

## 3. Public Interfaces

### 3.1 SkillDefinition

```rust
pub struct SkillDefinition {
    pub name: String,
    pub version: String,
    pub description: String,
    pub trigger: SkillTrigger,
    pub prompt_template: String,
    pub allowed_tools: Vec<String>,
    pub post_processing: Option<PostProcessing>,
    pub source: ArtifactSource,
    pub trust_level: TrustLevel,
    pub requirements: SkillRequirements,
    pub token_budget_hint: usize,
}
```

Field constraints:

- `version` must be recorded in Trace provenance
- `source` indicates whether the skill comes from the workspace, registry, or a local file
- `trust_level` decides whether hot reload, auto-execution, and the available tool ceiling are allowed
- `allowed_tools` is a hard constraint and cannot be bypassed by prompt text
- `token_budget_hint` participates in selection budgeting; it is not a display field

### 3.2 SkillTrigger

```rust
pub enum SkillTrigger {
    Command(String),
    Pattern(Regex),
    AgentDecision,
}
```

Default priority:

1. `Command`
2. `Pattern`
3. `AgentDecision`

### 3.3 SkillRequirements

```rust
pub struct SkillRequirements {
    pub required_bins: Vec<String>,
    pub required_env: Vec<String>,
    pub required_models: Vec<String>,
}
```

Semantics:

- `required_bins` checks for required local binaries
- `required_env` checks whether deployment environment variables are available
- `required_models` restricts activation to environments with the required model capabilities

### 3.4 SkillRegistry

```rust
pub struct SkillRegistry {
    skills: RwLock<HashMap<String, SkillDefinition>>,
    watcher: Option<FileWatcher>,
    selector: SkillSelector,
}

impl SkillRegistry {
    pub fn load_from_file(&self, path: &Path) -> Result<()>;
    pub fn load_from_dir(&self, dir: &Path) -> Result<()>;
    pub fn start_watching(&mut self, dir: &Path) -> Result<()>;
    pub fn get(&self, name: &str) -> Option<SkillDefinition>;
    pub fn list(&self) -> Vec<String>;
    pub fn select(&self, input: &Message, ctx: &SkillSelectionContext) -> Vec<SkillCandidate>;
}
```

### 3.5 SkillSelector

```rust
pub struct SkillSelector {
    pub max_prompt_tokens: usize,
    pub max_tools_for_installed: usize,
}
```

---

## 4. Implementation Details

### 4.1 Trust Model

Recommended default three-tier model:

- `Trusted`
  Skills placed in the user workspace or by an administrator. They may hot-reload and request a full tool set.
- `Installed`
  Skills installed through the registry. They may participate in automatic matching, but tool count and capabilities are downgraded.
- `Untrusted`
  Skills that may only be listed and reviewed, and cannot auto-execute.

Suggested rules:

- Only `Trusted` sources may hot-reload
- `Installed` skills should have a default tool-count limit
- Even if an `Untrusted` skill matches, it must stop for manual review or explicit approval before running

### 4.2 Selection Pipeline

Recommended order:

```text
gating -> scoring -> token budget -> tool ceiling attenuation -> final selection
```

Role of each stage:

- `gating`
  Filters out skills whose requirements are not satisfied
- `scoring`
  Scores candidates based on command matches, regex matches, description similarity, and so on
- `token budget`
  Ensures the total injected skill-description tokens stay within budget
- `tool ceiling attenuation`
  Lowers tool privileges and priority according to `trust_level`

### 4.3 Hot Reload Constraints

Hot reload is not unconditional:

- Watch only trusted directories
- Validate schema and check requirements before accepting file changes
- Record `name/version/source/hash` when replacing a version
- If a reload fails, keep the old version available rather than emptying the whole registry

### 4.4 Boundary with Tool Governance

`skills` can only declare which tools they want to call. They do not directly receive tool privileges. Before execution, the system still has to pass:

1. `SkillDefinition.allowed_tools`
2. The tool ceiling implied by `trust_level`
3. `ToolManifest.capabilities`
4. `sandbox` execution policy

That means the skill's allowlist is only one part of the upper bound, not the final execution authorization.

### 4.5 Trace and Audit

Every skill execution should record:

- `skill_name`
- `skill_version`
- `source`
- `trust_level`
- If installed from a registry, the registry artifact hash as well

Without this information, replay and audit will be distorted.

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `agent` | `AgentLoop` calls `SkillRegistry.select()` and executes skills |
| `tools` | Skills declare allowed tool sets but do not execute tools directly |
| `registry` | Supplies source, version, and hash metadata for installed skills |
| `trace` | Records skill version, source, and execution results |
| `workspace` | Provides trusted local skill directories and supports local hot reload |

---

## 6. Implementation Recommendations

- Build out `SkillDefinition` and the selection pipeline first, then add complex post-processing
- Do not hardcode the trust model in `agent`; governance rules should stay in `skills` as much as possible
- Make `load_from_dir()` resilient to partial failures
- Write table-driven tests for `select()` covering failed requirements, budget overflow, and low-trust downgrades
