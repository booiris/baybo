# Multi-Agent Chat — Phase 1 (binding + baybo consumption) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A web-chat session can be bound at creation to an `AgentProfile`, and the runtime consumes the binding: the profile's `system_prompt` replaces the Soul, its `llm` pin feeds LLM resolution, its skill folder overlays the shared registry, and memory recall/writes are partitioned by agent id.

**Architecture:** Spec: `docs/todo/multi-agent-chat.md`. Binding = two new flat columns on `sessions` (`agent_id`, `agent_framework`), written once at creation by a guarded targeted setter and patched onto `Session.state` on read (the `last_llm` pattern). The actor factory reads `session.state.agent_id` and wires a live `Arc<dyn AgentProfileStore>` lookup into `ContextManager` (prompt) and threads the agent id into `ToolContext` (skills scope + memory tools) and `MemoryContext` (partition key). Phase 2 (external-framework chat) is a separate plan; this phase **rejects** external-framework profiles at session creation.

**Tech Stack:** Rust workspace (tokio, axum/utoipa, libsql, DashMap, parking_lot), React/TS web app (openapi-typescript, vitest), ts-rs wire bindings.

## Global Constraints

- No `.unwrap()` / `.expect()` in production code (tests are fine). `parking_lot` locks only (no poisoning → no `.lock().unwrap()`).
- Deps: crate `Cargo.toml`s reference `{ workspace = true }` only; new external deps go in root `[workspace.dependencies]` first (this plan needs no new external dep).
- Zero clippy warnings including tests: `cargo clippy --all --benches --tests --examples --all-features -- -D warnings`. Test gate must NOT use `--all-features`: run `cargo nextest run --workspace` (or `-p <crate>` for scoped runs).
- Test-only helpers gated `#[cfg(test)]` or `#[cfg(any(test, feature = "test-support"))]` + a `test-support = []` feature; never plain `pub`.
- No magic strings: reuse `BUILTIN_AGENT_PROFILE_ID` (= `"baybo"`, in `baybo-model`) at every builtin-fallback site.
- Comments: only for non-obvious WHY. Follow surrounding style. No discarded-design archaeology.
- Timestamps in libsql: INTEGER µs via `super::time`. Booleans: INTEGER 0/1. Unknown enum string on read = error, never silent fallback.
- Migrations: append `ALTER TABLE ... ADD COLUMN` to the guarded `migrations` list in `crates/storage/src/libsql/mod.rs` AND add the column to the `CREATE TABLE` DDL (fresh DBs use CREATE, legacy use ALTER).
- OpenAPI regen after DTO change: `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync` then `pnpm --filter baybo-web gen:api`. Wire (ts-rs) regen: `scripts/check-ts-bindings.sh`.
- Session rows are core data — nothing here deletes or expires session rows.
- Commit after every task (pre-commit hook runs `cargo fmt --all --check`; run `cargo fmt` first).

---

### Task 1: Session model — binding fields + builtin fallback helper

**Files:**
- Modify: `crates/model/src/session.rs` (SessionState, ~line 253-324)

**Interfaces:**
- Produces: `SessionState.agent_id: Option<AgentProfileId>`, `SessionState.agent_framework: Option<AgentFramework>`, `SessionState::agent_id_or_builtin(&self) -> &str`. Every later task reads these.

- [ ] **Step 1: Write the failing tests**

In the existing `#[cfg(test)] mod tests` of `crates/model/src/session.rs`:

```rust
#[test]
fn session_state_agent_binding_defaults_none_and_round_trips() {
    let state = SessionState::default();
    assert_eq!(state.agent_id, None);
    assert_eq!(state.agent_framework, None);
    assert_eq!(state.agent_id_or_builtin(), BUILTIN_AGENT_PROFILE_ID);

    // Old blobs (no fields) still deserialize.
    let old: SessionState = serde_json::from_str("{}").unwrap();
    assert_eq!(old.agent_id, None);

    let mut bound = SessionState::default();
    bound.agent_id = Some(AgentProfileId::from("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
    bound.agent_framework = Some(AgentFramework::Baybo);
    assert_eq!(bound.agent_id_or_builtin(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    let json = serde_json::to_string(&bound).unwrap();
    let back: SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.agent_id, bound.agent_id);
    assert_eq!(back.agent_framework, Some(AgentFramework::Baybo));
}
```

Add needed imports to the test module: `use crate::agent_profile::{AgentFramework, AgentProfileId, BUILTIN_AGENT_PROFILE_ID};`

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-model session_state_agent_binding`
Expected: FAIL — `no field agent_id on SessionState` (compile error).

- [ ] **Step 3: Implement**

In `SessionState` (after the `last_llm` field, before `extra`):

```rust
    /// Agent-profile binding: which `agent_profiles` row this session was
    /// created under. `None` = the builtin `baybo` agent (all pre-binding
    /// rows). Written once at creation via the `set_agent_binding` targeted
    /// setter; patched from the flat `sessions.agent_id` column on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<crate::AgentProfileId>,

    /// Snapshot of the bound profile's framework at creation. Execution
    /// identity: a later profile-framework edit must not change how an
    /// existing transcript is served. `None` = baybo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_framework: Option<crate::AgentFramework>,
```

And on `impl SessionState`:

```rust
    /// The memory / display identity of this session's agent: the bound
    /// profile id, or the builtin id for unbound sessions.
    pub fn agent_id_or_builtin(&self) -> &str {
        self.agent_id
            .as_ref()
            .map(crate::AgentProfileId::as_str)
            .unwrap_or(crate::BUILTIN_AGENT_PROFILE_ID)
    }
```

(Confirm `AgentProfileId`, `AgentFramework`, `BUILTIN_AGENT_PROFILE_ID` are re-exported at the crate root — `crates/model/src/lib.rs` re-exports the `agent_profile` module items; if not, use `crate::agent_profile::...` paths.)

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p baybo-model`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(model): agent-profile binding fields on SessionState"
```

---

### Task 2: Storage — flat binding columns + write-once `set_agent_binding`

**Files:**
- Modify: `crates/store/src/session.rs` (trait, ~line 64)
- Modify: `crates/storage/src/libsql/mod.rs` (CREATE TABLE ~line 129-191; `migrations` list ~line 587)
- Modify: `crates/storage/src/libsql/session.rs` (`get` projection ~line 73-117, setter impls ~line 203, tests ~line 1284+)
- Modify: `crates/session/src/manager.rs` (wrapper, next to `set_last_llm` ~line 566)
- Modify: `crates/session/src/test_support.rs` (`MemorySessionStore` gains the trait method)

**Interfaces:**
- Consumes: Task 1's `SessionState` fields.
- Produces: `SessionStore::set_agent_binding(&self, session_id: &SessionId, agent_id: &AgentProfileId, framework: AgentFramework) -> Result<bool>` (false = missing row OR already bound — write-once); `SessionManager::set_agent_binding(...) -> Result<bool>` (same semantics, plus a `debug!` log). `SessionStore::get` patches `state.agent_id` / `state.agent_framework` from the columns.

- [ ] **Step 1: Write the failing tests**

In `#[cfg(test)] mod tests` of `crates/storage/src/libsql/session.rs` (reuse the existing `make_root_session` builder):

```rust
#[tokio::test]
async fn set_agent_binding_is_write_once_and_survives_save() {
    let pool = LibsqlPool::open_in_memory().await.unwrap();
    let store = LibsqlSessionStore::new(pool);
    let s = make_root_session("agent-bound");
    store.save(&s).await.unwrap();

    let id = baybo_model::AgentProfileId::from("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert!(
        store
            .set_agent_binding(&s.id, &id, baybo_model::AgentFramework::Baybo)
            .await
            .unwrap()
    );
    // Write-once: a second bind is refused.
    assert!(
        !store
            .set_agent_binding(&s.id, &id, baybo_model::AgentFramework::Baybo)
            .await
            .unwrap()
    );
    // Missing row is refused.
    let ghost = make_root_session("ghost");
    assert!(
        !store
            .set_agent_binding(&ghost.id, &id, baybo_model::AgentFramework::Baybo)
            .await
            .unwrap()
    );

    // A full-blob save (touch path) must not clobber the columns.
    store.save(&s).await.unwrap();
    let loaded = store.get(&s.id).await.unwrap().expect("row present");
    assert_eq!(loaded.state.agent_id, Some(id));
    assert_eq!(
        loaded.state.agent_framework,
        Some(baybo_model::AgentFramework::Baybo)
    );

    // Unbound session reads back as None.
    let plain = make_root_session("plain");
    store.save(&plain).await.unwrap();
    let loaded = store.get(&plain.id).await.unwrap().expect("row present");
    assert_eq!(loaded.state.agent_id, None);
    assert_eq!(loaded.state.agent_framework, None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-storage set_agent_binding`
Expected: FAIL — `no method named set_agent_binding` (compile error).

- [ ] **Step 3: Implement the trait method**

`crates/store/src/session.rs`, next to `set_last_llm` (import `AgentFramework, AgentProfileId` from `baybo_model`):

```rust
    /// Bind this session to an agent profile. Write-once: the SQL guard
    /// (`WHERE agent_id IS NULL`) makes a re-bind affect zero rows, so the
    /// binding is structurally immutable. `Ok(false)` = no row matched
    /// (missing id, or already bound).
    async fn set_agent_binding(
        &self,
        session_id: &SessionId,
        agent_id: &AgentProfileId,
        framework: AgentFramework,
    ) -> Result<bool>;
```

- [ ] **Step 4: Implement the libsql side**

1. `crates/storage/src/libsql/mod.rs` — add to the sessions `CREATE TABLE` (after `title`):

```sql
                    agent_id              TEXT,
                    agent_framework       TEXT,
```

2. Same file — append to the `migrations` list:

```rust
            "ALTER TABLE sessions ADD COLUMN agent_id TEXT",
            "ALTER TABLE sessions ADD COLUMN agent_framework TEXT",
```

3. `crates/storage/src/libsql/session.rs` — the setter (next to `set_last_llm`):

```rust
    async fn set_agent_binding(
        &self,
        session_id: &SessionId,
        agent_id: &AgentProfileId,
        framework: AgentFramework,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE sessions SET agent_id = ?2, agent_framework = ?3 \
                 WHERE id = ?1 AND agent_id IS NULL",
                libsql::params![
                    session_id.as_str().to_string(),
                    agent_id.as_str().to_string(),
                    framework.as_str(),
                ],
            )
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql set_agent_binding: {e}"))
            })?;
        Ok(affected > 0)
    }
```

4. Same file — extend **every** projection that patches flat columns over the blob. Find them:

Run: `command rg -n '"SELECT data' crates/storage/src/libsql/session.rs`

At each site (at minimum `get` and the list queries that already project `last_llm`), add `agent_id, agent_framework` to the SELECT column list and, after the existing patches (`session.state.last_llm = ...` etc.), apply:

```rust
                session.state.agent_id = agent_id_col.map(AgentProfileId::from);
                session.state.agent_framework = match agent_framework_col {
                    None => None,
                    Some(s) => Some(AgentFramework::parse(&s).ok_or_else(|| {
                        StorageError::Storage(format!("unknown agent_framework {s:?}"))
                    })?),
                };
```

where `agent_id_col: Option<String>` / `agent_framework_col: Option<String>` are read with the same `row.get` style as `last_llm_col` (indices shift — follow the existing pattern at `session.rs:73-117`). Unknown framework string is an **error**, matching the agent-profiles read rule.

5. `crates/session/src/manager.rs` — wrapper next to `set_last_llm`:

```rust
    /// Bind a session to an agent profile (write-once; see the store docs).
    /// Returns `Ok(false)` when the row is missing or already bound —
    /// callers decide whether that's an error.
    pub async fn set_agent_binding(
        &self,
        session_id: &SessionId,
        agent_id: &AgentProfileId,
        framework: AgentFramework,
    ) -> Result<bool> {
        let updated = self
            .store
            .set_agent_binding(session_id, agent_id, framework)
            .await?;
        debug!(session_id = %session_id, agent_id = %agent_id, updated, "set session agent binding");
        Ok(updated)
    }
```

6. `crates/session/src/test_support.rs` — implement the new trait method on `MemorySessionStore`: mutate the stored session's `state.agent_id`/`state.agent_framework` iff present and `state.agent_id.is_none()`, returning `Ok(true)`; else `Ok(false)`.

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p baybo-storage && cargo nextest run -p baybo-session && cargo nextest run -p baybo-store`
Expected: PASS (including all pre-existing anti-clobber tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(storage): sessions agent-binding columns + write-once setter"
```

---

### Task 3: Workspace — `agent-skills/` top-level dir

**Files:**
- Modify: `crates/workspace/src/paths.rs` (constants ~line 30-63, accessors ~line 351-397, doc diagram ~line 9-24, tests ~line 541+)
- Modify: `crates/workspace/src/manager.rs` (`ensure_layout` ~line 16-52, tests ~line 158+)

**Interfaces:**
- Produces: `pub const AGENT_SKILLS_DIR: &str = "agent-skills"`, `WorkspacePaths::agent_skills_dir(&self) -> PathBuf`. Dir is created and git-inited at every boot.

- [ ] **Step 1: Write the failing tests**

In `paths.rs` test `workspace_paths_compose_under_root`, add alongside the `skills_dir`/`agents_dir` asserts:

```rust
    assert_eq!(p.agent_skills_dir(), root.join("agent-skills"));
```

In `manager.rs` test `ensure_layout_creates_dirs_and_local_git_repos`, add `agent-skills` to the dirs-exist list AND to the `.git`-present asserts (it is declarative, versionable content like `skills/`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p baybo-workspace`
Expected: FAIL — `no method named agent_skills_dir`.

- [ ] **Step 3: Implement**

`paths.rs` — constant (in the top-level subdirectories block, after `AGENTS_DIR`):

```rust
/// Standalone git repo at `<root>/agent-skills/`: per-agent-profile skill
/// folders, one subdir per profile id (`agent-skills/<agent_id>/<skill>/
/// SKILL.md`). Skills here are visible only to chat sessions bound to that
/// agent, overlaid on the shared `skills/` set.
pub const AGENT_SKILLS_DIR: &str = "agent-skills";
```

Accessor (after `agents_dir`):

```rust
    pub fn agent_skills_dir(&self) -> PathBuf {
        self.root.join(AGENT_SKILLS_DIR)
    }
```

`manager.rs` — add `paths.agent_skills_dir(),` to BOTH arrays in `ensure_layout` (create list and git-init list). Update the layout diagram in `paths.rs` module docs and the `ensure_layout` doc comment to mention `agent-skills/`.

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p baybo-workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(workspace): agent-skills/ per-agent skill dir"
```

---

### Task 4: Skills — agent-scoped registry

**Files:**
- Modify: `crates/skills/src/registry.rs`
- Modify: `crates/baybo/src/runtime.rs` (registry wiring, ~line 245-261)

**Interfaces:**
- Consumes: Task 3's `WorkspacePaths::agent_skills_dir()`.
- Produces:
  - `SkillRegistry::load_agent_skills_root(&self, root: &Path) -> usize` — remembers the root, scans `<root>/<agent_id>/<skill>/SKILL.md`.
  - `SkillRegistry::get_scoped(&self, agent: Option<&str>, name: &str) -> Option<SkillDefinition>` — agent overlay first, then shared. `None` scope = shared only.
  - `SkillRegistry::summaries_for_agent(&self, agent: Option<&str>) -> Vec<SkillSummary>` — shared ∪ agent overlay, agent wins on name collision, name-sorted.
  - `reload()` also clears + rescans the agent root.

- [ ] **Step 1: Write the failing tests**

In `registry.rs`'s `#[cfg(test)] mod tests` (follow the existing tempdir + SKILL.md-writing helpers used by the `load_dir` tests; if the module has a `write_skill(dir, name, body)`-style helper, reuse it, otherwise write the file inline):

```rust
#[test]
fn agent_scoped_skills_overlay_shared_set() {
    let reg = SkillRegistry::new();
    let shared = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();

    // shared: `greet`, `deploy`
    for name in ["greet", "deploy"] {
        let d = shared.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: shared {name}\n---\nshared body {name}\n"),
        )
        .unwrap();
    }
    // agent A1: private `review` + overriding `greet`
    for name in ["review", "greet"] {
        let d = agents.path().join("A1").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: agent {name}\n---\nagent body {name}\n"),
        )
        .unwrap();
    }

    assert_eq!(reg.load_dir(shared.path()), 2);
    assert_eq!(reg.load_agent_skills_root(agents.path()), 2);

    // Unscoped view: shared only.
    let names: Vec<String> = reg
        .summaries_for_agent(None)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["deploy", "greet"]);
    assert_eq!(
        reg.get_scoped(None, "greet").unwrap().description,
        "shared greet"
    );
    assert!(reg.get_scoped(None, "review").is_none());

    // A1's view: shared ∪ overlay, overlay wins on collision, name-sorted.
    let a1: Vec<(String, String)> = reg
        .summaries_for_agent(Some("A1"))
        .into_iter()
        .map(|s| (s.name, s.description))
        .collect();
    assert_eq!(
        a1,
        [
            ("deploy".into(), "shared deploy".into()),
            ("greet".into(), "agent greet".into()),
            ("review".into(), "agent review".into()),
        ]
    );
    assert_eq!(
        reg.get_scoped(Some("A1"), "greet").unwrap().description,
        "agent greet"
    );
    // Unknown agent sees shared only.
    assert!(reg.get_scoped(Some("A2"), "review").is_none());

    // reload() replays both scans.
    std::fs::remove_dir_all(agents.path().join("A1").join("review")).unwrap();
    reg.reload();
    assert!(reg.get_scoped(Some("A1"), "review").is_none());
    assert_eq!(
        reg.get_scoped(Some("A1"), "greet").unwrap().description,
        "agent greet"
    );
}
```

(`tempfile` should already be a dev-dep of `baybo-skills`; if not, add `tempfile = { workspace = true }` to its `[dev-dependencies]`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-skills agent_scoped`
Expected: FAIL — missing methods.

- [ ] **Step 3: Implement**

`registry.rs` — struct gains two fields:

```rust
pub struct SkillRegistry {
    skills: DashMap<String, SkillDefinition>,
    /// Per-agent overlay skills keyed `(agent_id, skill_name)`; loaded from
    /// `<workspace>/agent-skills/<agent_id>/<skill>/SKILL.md`.
    agent_skills: DashMap<(String, String), SkillDefinition>,
    /// Directories passed to `load_dir`, in first-seen order, so `reload`
    /// can replay the same scans without callers tracking paths.
    load_dirs: RwLock<Vec<PathBuf>>,
    /// Root passed to `load_agent_skills_root`, replayed by `reload`.
    agent_skills_root: RwLock<Option<PathBuf>>,
}
```

Initialize both in `new()` (`DashMap::new()` / `RwLock::new(None)`). New methods:

```rust
    /// Scan `<root>/<agent_id>/<skill>/SKILL.md` into the per-agent overlay.
    /// Remembers `root` so `reload` replays the scan. Returns the number of
    /// agent skills loaded. A missing root is not an error (no agents have
    /// private skills yet).
    pub fn load_agent_skills_root(&self, root: &Path) -> usize {
        *self.agent_skills_root.write() = Some(root.to_path_buf());
        self.scan_agent_root(root)
    }

    fn scan_agent_root(&self, root: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(root) else {
            return 0;
        };
        let mut loaded = 0;
        for agent_entry in entries.flatten() {
            let agent_dir = agent_entry.path();
            if !agent_dir.is_dir() {
                continue;
            }
            let Some(agent_id) = agent_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Skip the git metadata of the agent-skills repo itself.
            if agent_id == ".git" {
                continue;
            }
            loaded += self.scan_agent_dir(&agent_id, &agent_dir);
        }
        loaded
    }
```

`scan_agent_dir(&self, agent_id: &str, dir: &Path) -> usize` mirrors the private `scan_dir` (depth-1 subdirs, require `<sub>/SKILL.md`, parse with the same loader fn `scan_dir` uses, log-and-skip broken files) but inserts via:

```rust
        self.agent_skills
            .insert((agent_id.to_owned(), skill.name.clone()), skill);
```

Scoped lookups:

```rust
    /// Scoped lookup: the agent's overlay first, then the shared set.
    /// `None` = shared set only (unbound / builtin sessions).
    pub fn get_scoped(&self, agent: Option<&str>, name: &str) -> Option<SkillDefinition> {
        if let Some(agent) = agent {
            if let Some(hit) = self
                .agent_skills
                .get(&(agent.to_owned(), name.to_owned()))
            {
                return Some(hit.value().clone());
            }
        }
        self.get(name)
    }

    /// Shared ∪ the agent's overlay (overlay wins on a name collision),
    /// sorted by name. `None` = shared only — equals `all_summaries_sorted`.
    pub fn summaries_for_agent(&self, agent: Option<&str>) -> Vec<SkillSummary> {
        let mut by_name: std::collections::BTreeMap<String, SkillSummary> = self
            .skills
            .iter()
            .map(|e| (e.key().clone(), SkillSummary::from(e.value())))
            .collect();
        if let Some(agent) = agent {
            for e in self.agent_skills.iter() {
                if e.key().0 == agent {
                    by_name.insert(e.key().1.clone(), SkillSummary::from(e.value()));
                }
            }
        }
        by_name.into_values().collect()
    }
```

`reload()` — extend to replay the agent scan:

```rust
    pub fn reload(&self) -> usize {
        let dirs: Vec<PathBuf> = self.load_dirs.read().clone();
        self.skills.clear();
        for dir in &dirs {
            self.scan_dir(dir);
        }
        let root = self.agent_skills_root.read().clone();
        self.agent_skills.clear();
        if let Some(root) = root {
            self.scan_agent_root(&root);
        }
        self.skills.len() + self.agent_skills.len()
    }
```

`crates/baybo/src/runtime.rs` — in the `skill_registry` block after `load_dir`:

```rust
    let agent_loaded = reg.load_agent_skills_root(&workspace_paths.agent_skills_dir());
    if agent_loaded > 0 {
        info!(count = agent_loaded, "loaded per-agent skills");
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p baybo-skills`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(skills): per-agent skill overlay in SkillRegistry"
```

---

### Task 5: Tools — `ToolContext.agent_id` + scoped Skill-tool lookup

**Files:**
- Modify: `crates/tools/src/lib.rs` (ToolContext, ~line 142-236)
- Modify: `crates/tools/src/test_support.rs` (fixture builder)
- Modify: `crates/skills/src/tools.rs` (Skill tool `execute`, ~line 138-207)
- Modify (compile-fix only, set `agent_id: None`): `crates/agent/src/runtime/tool_executor.rs:530`, `crates/context/src/background_summary.rs`, `crates/memory/tests/common/mod.rs`, plus any other `ToolContext {` literal the compiler flags (`command rg -ln 'ToolContext \{' crates/`)

**Interfaces:**
- Consumes: Task 4's `get_scoped`.
- Produces: `ToolContext.agent_id: Option<AgentProfileId>` (`None` = builtin/unbound). The Skill tool resolves through `get_scoped(ctx.agent_id.as_ref().map(|a| a.as_str()), name)`. Task 8 threads the real value; this task wires `None` everywhere.

- [ ] **Step 1: Write the failing test**

In `crates/skills/src/tools.rs`'s test module (reuse its existing SkillTool test scaffolding — registry + risk-check stub + `ToolContext` fixture; follow the shape of the existing "executes a skill" test):

```rust
#[tokio::test]
async fn skill_tool_resolves_agent_overlay_first() {
    // Registry with a shared `greet` and an A1 overlay `greet`.
    let registry = Arc::new(SkillRegistry::new());
    registry.register(make_skill("greet", "shared body"));
    let agents = tempfile::tempdir().unwrap();
    let d = agents.path().join("A1").join("greet");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("SKILL.md"),
        "---\nname: greet\ndescription: agent greet\n---\nagent body\n",
    )
    .unwrap();
    registry.load_agent_skills_root(agents.path());

    let tool = /* build SkillTool exactly like the existing tests */;
    let mut ctx = /* the module's existing ToolContext fixture */;
    ctx.agent_id = Some(baybo_model::AgentProfileId::from("A1"));

    let out = tool
        .execute(serde_json::json!({"skill": "greet"}), &ctx)
        .await
        .unwrap();
    let text = out.to_tool_result_text();
    assert!(text.contains("agent body"), "overlay must win: {text}");
}
```

(Adapt `make_skill` / fixture names to what the module already defines — the test module already builds `SkillTool`s and `ToolContext`s for the risk-gate tests; mirror those.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-skills skill_tool_resolves_agent_overlay`
Expected: FAIL — `no field agent_id on ToolContext`.

- [ ] **Step 3: Implement**

`crates/tools/src/lib.rs` — add to `ToolContext` (near `user`):

```rust
    /// The session's bound agent profile, if any. `None` = the builtin
    /// agent. Scopes per-agent skill lookups and defaults memory tools'
    /// agent namespace.
    pub agent_id: Option<baybo_model::AgentProfileId>,
```

`crates/tools/src/test_support.rs` — set `agent_id: None` in the fixture builder (all struct-literal fixtures get the field).

`crates/skills/src/tools.rs` — in `execute`, replace the lookup:

```rust
        let scope = ctx.agent_id.as_ref().map(|a| a.as_str());
        let skill = self
            .registry
            .get_scoped(scope, &p.skill)
            .ok_or_else(|| ToolError::NotFound(format!("skill '{}'", p.skill)))?;
```

Compile-fix every other `ToolContext {` literal with `agent_id: None` (`tool_executor.rs` gets the real value in Task 8; use `None` for now so this task compiles standalone).

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p baybo-tools && cargo nextest run -p baybo-skills && cargo build --workspace`
Expected: PASS / clean build.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(tools): agent identity on ToolContext; Skill tool scoped lookup"
```

---

### Task 6: Store test-support + Context — profile prompt arm + scoped skill listings

**Files:**
- Modify: `crates/store/Cargo.toml` + `crates/store/src/lib.rs` + Create: `crates/store/src/test_support.rs` (`MemoryAgentProfileStore`)
- Modify: `crates/context/Cargo.toml` (add `baybo-store`; dev-dep `baybo-store` with `test-support`)
- Modify: `crates/context/src/lib.rs` (config + fields ~line 192-285, `try_resolve_system_prompt` ~line 371, `invocable_skill_summaries` ~line 454, `insert_skill_trailer` ~line 1939, `expand_slash_command`/`slash_expansion_message` ~line 482-514, `build_skill_detail_payload` lookup)
- Modify (compile-fix `agent_profile: None`): every `ContextManagerConfig {` literal (`command rg -ln 'ContextManagerConfig \{' crates/`)

**Interfaces:**
- Consumes: `AgentProfileStore` trait, `AgentProfileRow` (existing); Task 4's scoped registry APIs.
- Produces:
  - `baybo_store::test_support::MemoryAgentProfileStore` (feature `test-support`): `new() -> Self` + `insert(row: AgentProfileRow)` + full `AgentProfileStore` impl over `parking_lot::Mutex<HashMap<AgentProfileId, AgentProfileRow>>`.
  - `ContextManagerConfig.agent_profile: Option<(Arc<dyn AgentProfileStore>, AgentProfileId)>` — live prompt lookups AND the skill scope (`scope = id.as_str()`).
  - Prompt priority: subagent profile > agent profile `system_prompt` > workspace Soul; profile row with `system_prompt: None` or a missing/erroring row **falls through to Soul** (never to the minimal fallback).

- [ ] **Step 1: Add `MemoryAgentProfileStore`**

`crates/store/Cargo.toml`:

```toml
[features]
test-support = []
```

(plus `parking_lot = { workspace = true }` under `[dependencies]` if not present). `crates/store/src/lib.rs`:

```rust
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
```

`crates/store/src/test_support.rs`:

```rust
//! In-memory fakes for the store traits (feature `test-support`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::AgentProfileId;
use parking_lot::Mutex;

use crate::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate};
use crate::error::Result;

/// In-memory [`AgentProfileStore`] for tests. No builtin seeding, no
/// name-uniqueness enforcement — insert exactly the rows the test needs.
#[derive(Default)]
pub struct MemoryAgentProfileStore {
    rows: Mutex<HashMap<AgentProfileId, AgentProfileRow>>,
}

impl MemoryAgentProfileStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, row: AgentProfileRow) {
        self.rows.lock().insert(row.id.clone(), row);
    }
}

#[async_trait]
impl AgentProfileStore for MemoryAgentProfileStore {
    async fn list(&self) -> Result<Vec<AgentProfileRow>> {
        Ok(self.rows.lock().values().cloned().collect())
    }
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>> {
        Ok(self.rows.lock().get(id).cloned())
    }
    async fn create(&self, row: &AgentProfileRow) -> Result<()> {
        self.rows.lock().insert(row.id.clone(), row.clone());
        Ok(())
    }
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.name = update.name.clone();
        row.description = update.description.clone();
        row.system_prompt = update.system_prompt.clone();
        row.framework = update.framework;
        row.llm = update.llm.clone();
        Ok(true)
    }
    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.avatar_blob_id = blob_id.map(str::to_owned);
        Ok(true)
    }
    async fn delete(&self, id: &AgentProfileId) -> Result<bool> {
        Ok(self.rows.lock().remove(id).is_some())
    }
}
```

(If `crate::error::Result` isn't the store crate's alias, use the same `Result` the `AgentProfileStore` trait file imports.)

- [ ] **Step 2: Write the failing context tests**

In `crates/context/src/lib.rs`'s test module (it already builds `ContextManager`s via `ContextManagerConfig` with `MemorySessionStore` — mirror an existing seed test; add `baybo-store = { workspace = true, features = ["test-support"] }` to `crates/context/Cargo.toml` `[dev-dependencies]`):

```rust
fn profile_row(id: &str, prompt: Option<&str>) -> baybo_store::agent_profile::AgentProfileRow {
    baybo_store::agent_profile::AgentProfileRow {
        id: baybo_model::AgentProfileId::from(id),
        name: id.to_string(),
        description: String::new(),
        avatar_blob_id: None,
        system_prompt: prompt.map(str::to_owned),
        framework: baybo_model::AgentFramework::Baybo,
        llm: None,
        builtin: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn agent_profile_prompt_overrides_soul_and_null_falls_through() {
    let store = baybo_store::test_support::MemoryAgentProfileStore::new();
    store.insert(profile_row("A1", Some("I am agent A1.")));
    store.insert(profile_row("A2", None));

    // Bound to A1 (prompt set) → profile prompt wins.
    let mut mgr = /* build via ContextManagerConfig exactly like the existing
                     seed tests, plus: */
        // agent_profile: Some((store.clone() as Arc<dyn AgentProfileStore>,
        //                      AgentProfileId::from("A1"))),
        ;
    mgr.ensure_seeded().await;
    let first = mgr.messages().first().unwrap();
    assert!(matches!(first.role, Role::System));
    let text = match &first.content[0] {
        ContentBlock::Text(t) => t.clone(),
        other => panic!("unexpected block {other:?}"),
    };
    assert_eq!(text, "I am agent A1.");

    // Bound to A2 (NULL prompt) → workspace Soul (contains the soul TOP hint,
    // not the minimal fallback).
    let mut mgr2 = /* same, agent id "A2" */;
    mgr2.ensure_seeded().await;
    let text2 = /* first system row text as above */;
    assert_ne!(text2, "I am agent A1.");
    assert!(text2.contains("<soul"), "NULL prompt must inherit the Soul: {text2}");

    // Bound to a deleted profile → Soul fallback too.
    let mut mgr3 = /* same, agent id "GONE" */;
    mgr3.ensure_seeded().await;
    let text3 = /* first system row text */;
    assert!(text3.contains("<soul"), "missing profile must inherit the Soul");
}
```

Also add a scoped-skill-listing test: build a registry with a shared skill + an `A1` overlay skill (as in Task 4's test), construct a manager bound to `A1`, call `ensure_seeded`, and assert the second appended message (the skill reminder) mentions the overlay skill; an unbound manager's reminder must not.

And a live-edit test (the spec's "profile edit → reseed picks it up"): seed a manager bound to `A1`, then `store.update(&id, &AgentProfileUpdate { system_prompt: Some("edited".into()), name: "A1".into(), description: String::new(), framework: AgentFramework::Baybo, llm: None }).await`, and assert a re-resolution returns the edited prompt. Mirror however the existing tests exercise the soul-edit reseed path (search the test module for `reseed`); if reseed is only reachable through compaction there, assert via a second manager constructed with the same store — the load-bearing property is that resolution is a live store read, not a snapshot.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo nextest run -p baybo-context agent_profile`
Expected: FAIL — `no field agent_profile on ContextManagerConfig`.

- [ ] **Step 4: Implement**

`crates/context/Cargo.toml` `[dependencies]`: add `baybo-store = { workspace = true }` (recon confirmed: store is a leaf crate, no cycle; context already pulls it transitively via `baybo-session`).

`ContextManagerConfig` + `ContextManager` both gain (mirroring `subagent_profile`'s comment style):

```rust
    /// For an agent-bound session: `(profile store, profile id)` — context
    /// resolves the session's system prompt from the profile row live (seed
    /// + every post-compaction reseed, so a profile edit lands like a Soul
    /// edit) and scopes skill listings to the agent's overlay folder.
    /// `None` = builtin behavior (workspace Soul, shared skills only).
    pub agent_profile: Option<(Arc<dyn baybo_store::agent_profile::AgentProfileStore>, AgentProfileId)>,
```

Copy the field in `from_config`. Private helper:

```rust
    fn agent_scope(&self) -> Option<&str> {
        self.agent_profile.as_ref().map(|(_, id)| id.as_str())
    }
```

`try_resolve_system_prompt` — insert the middle arm (subagent arm unchanged and still first; restructure the `match` into early returns):

```rust
    async fn try_resolve_system_prompt(&self) -> Option<String> {
        if let Some((registry, profile_name)) = &self.subagent_profile {
            let resolved = registry.get(profile_name).map(|p| p.system_prompt);
            if resolved.is_none() {
                tracing::warn!(subagent_type = %profile_name, "subagent profile not found in registry");
            }
            return resolved;
        }
        if let Some((store, profile_id)) = &self.agent_profile {
            match store.get(profile_id).await {
                Ok(Some(row)) => {
                    if let Some(prompt) = row.system_prompt {
                        return Some(prompt);
                    }
                    // NULL prompt ⇒ inherit the workspace Soul below.
                }
                Ok(None) => tracing::warn!(
                    agent_id = %profile_id,
                    "bound agent profile missing; falling back to workspace soul"
                ),
                Err(e) => tracing::warn!(
                    agent_id = %profile_id, error = %e,
                    "agent profile lookup failed; falling back to workspace soul"
                ),
            }
        }
        match crate::prompts::soul::assemble_from_workspace(&self.workspace).await {
            Ok(prompt) => Some(prompt),
            Err(e) => {
                tracing::warn!(error = %e, "failed to assemble workspace soul");
                None
            }
        }
    }
```

(`reseed_system_row` calls this, so live profile edits land after each compaction with no further change.)

Skill scoping — replace each `all_summaries_sorted()` consumer:

- `invocable_skill_summaries`: `self.skill_registry.summaries_for_agent(self.agent_scope())`.
- `insert_skill_trailer`: add a `agent: Option<&str>` parameter, use `registry.summaries_for_agent(agent)`; the `maybe_compress` call site passes `self.agent_scope()`. If `build_skill_detail_payload` looks skills up via `registry.get`, give it the same `agent` parameter and switch to `get_scoped`.
- `slash_expansion_message`: switch `skill_registry.get(&skill_name)` to `skill_registry.get_scoped(self.agent_scope(), &skill_name)` (the candidate list already comes from `invocable_skill_summaries`).

Compile-fix every other `ContextManagerConfig {` literal with `agent_profile: None`.

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p baybo-context && cargo nextest run -p baybo-store && cargo build --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(context): agent-profile system prompt + agent-scoped skill listings"
```

---

### Task 7: Memory — partition every hook and tool by agent id

**Files:**
- Modify: `crates/memory/src/lib.rs` (MemoryContext ~line 55-93)
- Modify: `crates/memory/src/backends/mem0.rs` (recall body, `add_turn` ~line 534, tools ~lines 830-1390)
- Modify: `crates/memory/src/backends/openviking.rs` (`base_headers` ~line 227, call sites 353/375/392/412/606, tools ~line 997+)
- Modify: `crates/memory/src/test_support.rs` (`RecordingMemory` records agent ids)
- Modify: `crates/memory/tests/common/mod.rs` (fixtures), `crates/memory/tests/mem0.rs`, `crates/memory/tests/openviking.rs`
- Modify (call sites): `crates/agent/src/runtime/agent_loop.rs` (~lines 1691-1706, 1812-1826, 1861-1877)

**Interfaces:**
- Consumes: Task 1's `SessionState::agent_id_or_builtin()`; Task 5's `ToolContext.agent_id`.
- Produces: `MemoryContext::new(user_id: String, agent_id: String, session_id: SessionId, job_id: JobId, recorder: Arc<SpanRecorder>, step: StepHandle)` + `pub fn agent_id(&self) -> &str`. mem0 writes carry `agent_id`, mem0 recall filters `AND [{user_id}, {agent_id}]`; OpenViking sends `X-OpenViking-Agent: <agent_id>`. Memory tools default their agent namespace to `ctx.agent_id` (explicit `agentId` param still overrides).

- [ ] **Step 1: Write the failing backend tests**

In `crates/memory/tests/openviking.rs`, clone the existing `recall_sends_query_and_returns_abstract_with_uri` test into:

```rust
#[tokio::test]
async fn recall_sends_bound_agent_header() {
    // identical server + backend setup, but build the MemoryContext with
    // agent_id "A1" (extend the common::memory_context fixture — see step 3)
    // then:
    let headers = captured.headers.lock().last().cloned().unwrap();
    assert_eq!(headers.get("x-openviking-agent").unwrap(), "A1");
}
```

In `crates/memory/tests/mem0.rs`, add:

```rust
#[tokio::test]
async fn on_job_complete_and_recall_carry_agent_id() {
    // same capture-server pattern as the existing mem0 tests, ctx agent "A1":
    // after on_job_complete:
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["agent_id"], "A1");
    // after recall: the search body's AND filters include {"agent_id": "A1"}
    let filters = body["filters"]["AND"].as_array().unwrap();
    assert!(filters.iter().any(|f| f["agent_id"] == "A1"), "{filters:?}");
}
```

Keep (or add) one assertion that a ctx built without a binding still sends `"baybo"` — the pre-existing `x-openviking-agent == "baybo"` assertion covers openviking; add the mem0 write equivalent if absent.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p baybo-memory`
Expected: FAIL — fixture/`MemoryContext::new` signature mismatch once you extend the fixture; drive it from there.

- [ ] **Step 3: Implement**

`crates/memory/src/lib.rs`:

```rust
pub struct MemoryContext {
    user_id: String,
    agent_id: String,
    session_id: SessionId,
    job_id: JobId,
    recorder: Arc<SpanRecorder>,
    step: StepHandle,
}
```

`new` gains `agent_id: String` as the second parameter; add accessor:

```rust
    /// The bound agent-profile id, or the builtin id (`"baybo"`) for
    /// unbound sessions. Backends use it as their partition key.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
```

`crates/memory/tests/common/mod.rs` — extend the `memory_context(...)` fixture with an agent parameter (add a `memory_context_for_agent(agent: &str, ...)` variant if the existing signature is widely used; default variant passes `"baybo"`). Update `crates/memory/src/test_support.rs` `RecordingMemory` to record `ctx.agent_id().to_owned()` alongside what it already captures per call (extend its recorded tuples/structs and any accessors).

**mem0** (`backends/mem0.rs`):
- `recall`: add `{"agent_id": ctx.agent_id()}` to the search body's `filters.AND` array alongside `{user_id}`.
- `on_job_complete` → `add_turn`: add an `agent_id: &str` parameter, body `"agent_id": agent_id` (replacing `DEFAULT_AGENT_ID`); pass `ctx.agent_id()`.
- Tools: at each tool's identity extraction (`Mem0AddTool::execute` ~line 959-963, search ~834, list ~1114, delete ~1308):

```rust
        let ctx_agent = ctx
            .agent_id
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or(DEFAULT_AGENT_ID);
        let agent_id = params
            .get("agentId")
            .and_then(|v| v.as_str())
            .unwrap_or(ctx_agent);
```

For the read tools that today pass `None` into `build_filters` when `agentId` is absent, pass `Some(agent_id)` instead (reads default-scope to the session's agent; the explicit param still overrides). Update the `agentId` schema descriptions from "default: baybo" to "default: the session's agent".

**openviking** (`backends/openviking.rs`):
- `base_headers(&self, user_id: &str)` → `base_headers(&self, user_id: &str, agent_id: &str)`; the header insert uses `agent_id` (keep `DEFAULT_AGENT` for fallbacks). Update `json_headers` the same way.
- Hook call sites (353/375/392/412) pass `ctx.agent_id()`; the health check (606) passes `DEFAULT_AGENT`.
- Viking tools: pass `ctx.agent_id.as_ref().map(|a| a.as_str()).unwrap_or(DEFAULT_AGENT)`.

**agent loop** (`crates/agent/src/runtime/agent_loop.rs`) — at each of the three mint sites, alongside the existing `let user_id = session.user.id.clone();`:

```rust
        let agent_id = session.state.agent_id_or_builtin().to_owned();
```

and pass it as the second `MemoryContext::new` argument. (For `spawn_session_end_write`, the session is available where `user_id` is sourced — capture `agent_id` the same way.)

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p baybo-memory && cargo nextest run -p baybo-agent && cargo build --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(memory): partition recall/writes and memory tools by agent id"
```

---

### Task 8: Agent runtime — binding resolution at spawn + LLM precedence + identity threading

**Files:**
- Modify: `crates/agent/src/actor/router/mod.rs` (`Router` ~line 157, `RouterConfig` ~line 182)
- Modify: `crates/agent/src/actor/router/user_input.rs` (`handle_incoming` ~line 137-160, `handle_incoming_batch` ~line 322)
- Modify: `crates/agent/src/runtime/tool_executor.rs` (`execute` params ~line 321-344, ToolContext fill ~line 530)
- Modify: `crates/agent/src/runtime/agent_loop.rs` (executor call sites ~line 1178-1210 and the slash/background paths the compiler flags)
- Modify: `crates/baybo/src/runtime.rs` (actor factory ~line 808-885, RouterConfig ~line 961)
- Modify: `crates/agent/tests/*` + `crates/integration-tests/*` as the compiler flags (fixtures gain `agent_profile_store` / `agent_id` values)

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `RouterConfig.agent_profile_store: Arc<dyn AgentProfileStore>` (+ same field on `Router`).
  - `resolve_initial_llm(store, session) -> Option<LlmEntryName>`: `last_llm` wins; else bound profile's `llm` (live read); else `None` (pool default). Errors/missing rows degrade to `None` with `warn!`.
  - `ToolExecutor::execute` gains `agent_id: Option<AgentProfileId>` (passed into `ToolContext`).
  - The actor factory wires `ContextManagerConfig.agent_profile` from `session.state.agent_id`.
- Note: profile-`llm` liveness is **spawn-time** (next hydration after idle reap), not per-turn — Task 11 records this in the spec. An explicit user model switch (`last_llm`) always wins immediately.

- [ ] **Step 1: Write the failing router test**

In `crates/agent/src/actor/router/user_input.rs`'s (or the router test module's) `#[cfg(test)]` — a pure unit test of the precedence helper:

```rust
#[tokio::test]
async fn initial_llm_prefers_session_pin_then_profile_pin() {
    use baybo_store::test_support::MemoryAgentProfileStore;
    let store = MemoryAgentProfileStore::new();
    store.insert(/* profile_row helper as in Task 6, but with
                    llm: Some(LlmEntryName::from("profile-pin")) and id "A1" */);
    let store: Arc<dyn baybo_store::agent_profile::AgentProfileStore> = store;

    let mut session = /* the crate's existing test-session builder
                         (rg "fn make_session" crates/agent) */;

    // Unbound, no pin → None (pool default).
    assert_eq!(resolve_initial_llm(&store, &session).await, None);

    // Bound, no explicit pin → profile pin.
    session.state.agent_id = Some(baybo_model::AgentProfileId::from("A1"));
    assert_eq!(
        resolve_initial_llm(&store, &session).await,
        Some(LlmEntryName::from("profile-pin"))
    );

    // Explicit session pin always wins.
    session.state.last_llm = Some(LlmEntryName::from("user-pick"));
    assert_eq!(
        resolve_initial_llm(&store, &session).await,
        Some(LlmEntryName::from("user-pick"))
    );

    // Deleted profile degrades to default.
    session.state.last_llm = None;
    session.state.agent_id = Some(baybo_model::AgentProfileId::from("GONE"));
    assert_eq!(resolve_initial_llm(&store, &session).await, None);
}
```

Add `baybo-store = { workspace = true, features = ["test-support"] }` to `crates/agent/Cargo.toml` `[dev-dependencies]` (the plain dep may already exist for `TaskStore`; features merge across dep sections is fine because `test-support` is additive).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-agent initial_llm_prefers`
Expected: FAIL — `resolve_initial_llm` not found.

- [ ] **Step 3: Implement the router side**

`user_input.rs` (module scope):

```rust
/// Effective initial LLM pin for a spawning actor: the session's explicit
/// pin wins; otherwise the bound agent profile's pin (read live, so a
/// profile edit lands on the next hydration); otherwise pool default.
pub(crate) async fn resolve_initial_llm(
    store: &Arc<dyn AgentProfileStore>,
    session: &Session,
) -> Option<LlmEntryName> {
    if let Some(pin) = &session.state.last_llm {
        return Some(pin.clone());
    }
    let agent_id = session.state.agent_id.as_ref()?;
    match store.get(agent_id).await {
        Ok(Some(profile)) => profile.llm,
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id, error = %e,
                "agent profile llm lookup failed; using default-llm"
            );
            None
        }
    }
}
```

`Router` + `RouterConfig` gain `agent_profile_store: Arc<dyn AgentProfileStore>` (copy through `from_config` like `session_manager`). In `handle_incoming`, the profile read is async but the spawn closure is sync — so hoist it: immediately BEFORE the `route_or_spawn` call add

```rust
        let pinned = resolve_initial_llm(&self.agent_profile_store, &session).await;
```

and inside the spawn closure delete `let pinned = session.state.last_llm.clone();`, letting the closure move the hoisted `pinned` instead. Apply the same change at the `handle_incoming_batch` site (~line 322).

`crates/baybo/src/runtime.rs` RouterConfig literal (~line 961): add `agent_profile_store: Arc::clone(&<store bundle>.agent_profile),` — bind from the same `Store` bundle the runtime already exposes (the local that provides `task_store`; find it with `command rg -n "task_store" crates/baybo/src/runtime.rs`).

- [ ] **Step 4: Wire the actor factory + executor threading**

`crates/baybo/src/runtime.rs`, in `spawn_actor_for`'s closure (~line 830): capture `let agent_profile_store: Arc<dyn AgentProfileStore> = Arc::clone(&<store bundle>.agent_profile);` outside the closure, then inside the `ContextManagerConfig` literal add:

```rust
                        agent_profile: session
                            .state
                            .agent_id
                            .clone()
                            .map(|id| (Arc::clone(&agent_profile_store), id)),
```

`crates/agent/src/runtime/tool_executor.rs` — `execute` gains a parameter after `user: &User`:

```rust
        agent_id: Option<AgentProfileId>,
```

and the `ToolContext` literal (~line 530) sets `agent_id,` (replacing Task 5's `None`).

`crates/agent/src/runtime/agent_loop.rs` — at the executor call sites, alongside `let user_for_calls = session.user.clone();`:

```rust
        let agent_for_calls = session.state.agent_id.clone();
```

clone per closure like `user`, and pass to `executor.execute(...)` in the new position. Fix every other `.execute(` caller the compiler flags the same way (pass `None` where no session is in scope, e.g. background-summary utility paths).

- [ ] **Step 5: Add the memory-partition e2e**

In `crates/integration-tests/tests/agent_loop_e2e.rs` (mirror an existing harness-driven case): seed a session whose `state.agent_id = Some(AgentProfileId::from("A1"))` via `MemorySessionStore::seed_session`, wire a `RecordingMemory` (baybo-memory test-support) into the harness's memory slot, run one user turn through the harness, and assert the recorded `recall` + `on_job_complete` calls carry `agent_id == "A1"`, while a turn on an unbound session records `"baybo"`. (If the harness builder lacks a memory or session-seeding knob, add the minimal builder method following its existing `with_*` style.)

- [ ] **Step 6: Run tests**

Run: `cargo nextest run -p baybo-agent && cargo nextest run -p baybo && cargo nextest run --workspace`
Expected: PASS (workspace run catches fixture fallout in `integration-tests`; fix flagged literals with `agent_id: None` / builder defaults).

- [ ] **Step 7: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(agent): resolve agent binding at spawn — prompt, llm precedence, identity threading"
```

---

### Task 9: Gateway — create-with-agent API, DTO fields, scoped /v1/skills

**Files:**
- Modify: `crates/gateway/src/api/admin/chat.rs` (CreateSessionRequest ~line 194, parse ~line 1792, create_session ~line 642, create_or_load_chat_session ~line 1809, ChatSessionSummary ~line 525 + build ~line 755, ChatSessionDetail ~line 489 + build ~line 802)
- Modify: `crates/gateway/src/api/admin/skills.rs` (query param)
- Modify: `crates/wire/src/lib.rs` (SessionPatch ~line 887-935)
- Modify: `crates/gateway/tests/chat_api.rs`
- Regen: `docs/openapi.json`, `app/web/src/api/schema.d.ts`, `sidecars/sdk/channel-ts/src/generated/`

**Interfaces:**
- Consumes: Task 2's `SessionManager::set_agent_binding`; Task 4's `summaries_for_agent`.
- Produces:
  - `POST /v1/chat/sessions` accepts `agent_id?: string`. `"baybo"`/empty ⇒ unbound; unknown id ⇒ 400; non-`baybo` framework ⇒ 400 (Phase 2 lifts this); `agent_id` + an already-existing `session_id` ⇒ 400.
  - `ChatSessionSummary` / `ChatSessionDetail` / wire `SessionPatch` gain `agent_id?: string` (+ `agent_framework?: string` on the two REST DTOs).
  - `GET /v1/skills?agent_id=<id>` returns the merged scoped listing.

- [ ] **Step 1: Write the failing gateway tests**

In `crates/gateway/tests/chat_api.rs` (reuse `build_test_deps` + the local `post`/`get` helpers; the real libsql store seeds the builtin profile, and profiles can be created through `POST /v1/agents` exactly as `agents_api.rs` does):

```rust
#[tokio::test]
async fn create_session_binds_agent_profile() {
    // setup identical to chat_api_round_trip
    // 1. create a baybo-framework profile via POST /v1/agents {"name": "helper"}
    //    → capture its id.
    // 2. POST /v1/chat/sessions {"agent_id": <id>} → 200, session_id.
    // 3. GET /v1/chat/sessions/<sid> → detail.agent_id == <id>,
    //    detail.agent_framework == "baybo".
    // 4. GET /v1/chat/sessions → the row for <sid> carries agent_id == <id>.
    // 5. POST /v1/chat/sessions {"agent_id": "baybo"} → 200; its detail has
    //    NO agent_id (builtin normalizes to unbound).
    // 6. POST /v1/chat/sessions {"agent_id": "nope"} → 400.
    // 7. POST /v1/agents {"name": "ext", "framework": "claude"} → id;
    //    POST /v1/chat/sessions {"agent_id": <ext-id>} → 400 mentioning
    //    "not supported yet".
    // 8. POST /v1/chat/sessions {"session_id": <sid>, "agent_id": <id>}
    //    (existing session) → 400.
}
```

Write it as real request/assert code following the file's established helper style — every step above is one `post`/`get` call plus `serde_json` field asserts.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-gateway create_session_binds`
Expected: FAIL — unknown field `agent_id` is silently ignored, so assertion 3 fails first.

- [ ] **Step 3: Implement the chat API**

`CreateSessionRequest` gains:

```rust
    /// Agent profile to bind the new session to. Omitted, `null`, or the
    /// builtin id ⇒ the builtin agent (no binding). Only valid when the
    /// call actually creates a session.
    #[serde(default)]
    pub agent_id: Option<String>,
```

`parse_create_session_request`: trim `agent_id` like `session_id`; empty-after-trim → `BadRequest("agent_id must not be empty")`.

New helper in `chat.rs`:

```rust
/// Resolve a requested agent binding at session creation. `None` ⇒ builtin
/// (unbound). Write-time validation: unknown id and external frameworks are
/// crisp 400s; the builtin id normalizes to `None` so `NULL` stays the single
/// representation of "builtin".
async fn resolve_agent_binding(
    state: &AdminState,
    agent_id: Option<&str>,
) -> Result<Option<(AgentProfileId, AgentFramework)>> {
    let Some(raw) = agent_id else { return Ok(None) };
    if raw == BUILTIN_AGENT_PROFILE_ID {
        return Ok(None);
    }
    let id = AgentProfileId::from(raw);
    let profile = state
        .agent_profile_store
        .get(&id)
        .await
        .map_err(|e| GatewayError::Internal(format!("load agent profile: {e}")))?
        .ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown agent_id {raw:?}; see GET /v1/agents"))
        })?;
    if profile.framework != AgentFramework::Baybo {
        return Err(GatewayError::BadRequest(format!(
            "agent {:?} runs framework {:?}; external-framework chat sessions are not supported yet",
            profile.name,
            profile.framework.as_str(),
        )));
    }
    Ok(Some((id, profile.framework)))
}
```

`create_session`: call `let binding = resolve_agent_binding(&state, requested.agent_id.as_deref()).await?;` before `create_or_load_chat_session`, pass `binding` in as a new parameter, and include `agent_id` in the created broadcast patch (see wire change below). `create_or_load_chat_session(state, requested_session_id, user, channel_type, binding: Option<(AgentProfileId, AgentFramework)>)`:

- The load-existing arm (`return Ok(existing)`): if `binding.is_some()`, `return Err(GatewayError::BadRequest("cannot set an agent on an existing session".into()))`.
- After EACH freshly-created session (both the no-id and requested-id arms), stamp + mirror in-memory:

```rust
    if let Some((agent_id, framework)) = binding {
        let updated = state
            .session_manager
            .set_agent_binding(&session.id, &agent_id, framework)
            .await
            .map_err(|e| GatewayError::Internal(format!("bind agent to session: {e}")))?;
        if !updated {
            return Err(GatewayError::Internal(
                "session vanished or was already agent-bound".to_owned(),
            ));
        }
        session.state.agent_id = Some(agent_id);
        session.state.agent_framework = Some(framework);
    }
    Ok(session)
```

(restructure the fn so both fresh arms flow through this tail; make `session` mutable).

**DTOs** — `ChatSessionSummary` and `ChatSessionDetail` both gain:

```rust
    /// Bound agent profile id; absent = the builtin agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Framework snapshot of the binding (`baybo` in Phase 1); absent = baybo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_framework: Option<String>,
```

Fill at the inline build sites:

```rust
            agent_id: s.state.agent_id.as_ref().map(|a| a.to_string()),
            agent_framework: s.state.agent_framework.map(|f| f.as_str().to_owned()),
```

(`s` = the session/`session` binding at each site.)

**Wire** — `crates/wire/src/lib.rs` `SessionPatch` gains (matching the existing optional-field attribute style):

```rust
    /// Bound agent profile id. Emitted on the create broadcast so sibling
    /// tabs can render the agent chip without a refetch; never changes later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub agent_id: Option<String>,
```

`create_session`'s broadcast fills `agent_id: session.state.agent_id.as_ref().map(|a| a.to_string())`; every other `SessionPatch { .. }` literal adds `agent_id: None` (compiler-driven; `Default` covers `..Default::default()` sites).

**Skills endpoint** — `crates/gateway/src/api/admin/skills.rs`:

```rust
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SkillsQuery {
    /// Scope the listing to this agent profile's skill folder overlaid on
    /// the shared set. Omitted or the builtin id ⇒ shared set only.
    pub agent_id: Option<String>,
}
```

Handler takes `Query(q): Query<SkillsQuery>`, add `params(SkillsQuery)` to the utoipa attribute, and:

```rust
    let scope = q
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != BUILTIN_AGENT_PROFILE_ID);
    let items = state
        .skill_registry
        .summaries_for_agent(scope)
        .into_iter()
        .map(|s| SkillInfo { name: s.name, description: s.description })
        .collect();
```

- [ ] **Step 4: Regenerate schemas + run tests**

```bash
UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync
pnpm --filter baybo-web gen:api
scripts/check-ts-bindings.sh
cargo nextest run -p baybo-gateway
```

Expected: regen files change; gateway tests PASS (including the new one and the pre-existing openapi drift test).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(gateway): create chat sessions bound to an agent profile"
```

---

### Task 10: Web — agent picker, session agent chip, scoped skills readout

**Files:**
- Create: `app/web/src/pages/chat/AgentPicker.tsx`
- Create: `app/web/src/pages/chat/AgentPicker.test.tsx`
- Create: `app/web/src/api/useBlobUrl.ts` (extracted from AgentsPage)
- Modify: `app/web/src/pages/AgentsPage.tsx` (import the extracted hook; scoped skills fetch)
- Modify: `app/web/src/pages/ChatPage.tsx` (`handleNewChat` ~line 2549, header ~line 2615, agents fetch)
- Modify: `app/web/src/pages/chat/SessionSidebar.tsx` (new-chat button ~line 764, row ~line 140)
- Modify: `app/web/src/pages/chat/types.ts` (`SessionSummary`)

**Interfaces:**
- Consumes: Task 9's `agent_id` on create/list/detail + `GET /v1/skills?agent_id`; existing `GET /v1/agents`.
- Produces: `AgentPicker({ agents, onPick, onClose })` — popover listing profiles (builtin first, preselected), `onPick(agentId: string | null)` where `null` = builtin. `SessionSummary.agent_id?: string`.

- [ ] **Step 1: Write the failing component test**

`app/web/src/pages/chat/AgentPicker.test.tsx` (vitest + jsdom, matching existing test setup under `app/web`):

```tsx
import { render, screen, fireEvent } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentPicker, type AgentOption } from './AgentPicker';

const agents: AgentOption[] = [
  { id: 'baybo', name: 'baybo', description: 'default', builtin: true, framework: 'baybo' },
  { id: 'A1', name: 'Helper', description: 'helps', builtin: false, framework: 'baybo' },
  { id: 'A2', name: 'Coder', description: 'claude-backed', builtin: false, framework: 'claude' },
];

describe('AgentPicker', () => {
  it('lists agents builtin-first and picks by click', () => {
    const onPick = vi.fn();
    render(<AgentPicker agents={agents} onPick={onPick} onClose={() => {}} />);
    const rows = screen.getAllByRole('button');
    expect(rows[0]).toHaveTextContent('baybo');
    fireEvent.click(screen.getByText('Helper'));
    expect(onPick).toHaveBeenCalledWith('A1');
  });

  it('maps the builtin to null and disables external-framework agents', () => {
    const onPick = vi.fn();
    render(<AgentPicker agents={agents} onPick={onPick} onClose={() => {}} />);
    fireEvent.click(screen.getByText('baybo'));
    expect(onPick).toHaveBeenCalledWith(null);
    expect(screen.getByText('Coder').closest('button')).toBeDisabled();
  });
});
```

(If `@testing-library/react` / `@testing-library/jest-dom` are not yet devDeps of `app/web`, add them via `pnpm --filter baybo-web add -D @testing-library/react @testing-library/jest-dom` and wire the jest-dom matchers in the vitest setup file; check `app/web/vitest.config.ts` for an existing `setupFiles`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `pnpm --filter baybo-web test`
Expected: FAIL — module `./AgentPicker` not found.

- [ ] **Step 3: Implement AgentPicker**

`app/web/src/pages/chat/AgentPicker.tsx` — a presentational popover styled like the sidebar (brutal borders / `bg-canvas`, follow `SessionSidebar.tsx` classes):

```tsx
export interface AgentOption {
  id: string;
  name: string;
  description: string;
  builtin: boolean;
  framework: string;
}

export function AgentPicker({
  agents,
  onPick,
  onClose,
}: {
  agents: AgentOption[];
  onPick: (agentId: string | null) => void;
  onClose: () => void;
}) {
  const ordered = [...agents].sort((a, b) =>
    a.builtin === b.builtin ? a.name.localeCompare(b.name) : a.builtin ? -1 : 1,
  );
  return (
    <div
      role="menu"
      className="absolute z-30 mt-1 w-full bg-canvas border-2 border-black rounded-md shadow-brutal-sm p-1 flex flex-col gap-1"
      onMouseLeave={onClose}
    >
      {ordered.map((a) => {
        const external = a.framework !== 'baybo';
        return (
          <button
            key={a.id}
            type="button"
            disabled={external}
            title={external ? 'External-framework chat is not supported yet' : a.description}
            onClick={() => onPick(a.builtin ? null : a.id)}
            className="flex items-center gap-2 px-2 py-1.5 text-left rounded hover:bg-brand/20 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <span className="w-6 h-6 shrink-0 rounded-full border-2 border-black bg-brand/40 flex items-center justify-center text-[0.7rem] font-bold uppercase">
              {a.name.slice(0, 1)}
            </span>
            <span className="min-w-0">
              <span className="block text-sm font-bold truncate">{a.name}</span>
              {a.description ? (
                <span className="block text-[0.7rem] text-ink-soft truncate">{a.description}</span>
              ) : null}
            </span>
          </button>
        );
      })}
    </div>
  );
}
```

- [ ] **Step 4: Wire ChatPage + sidebar + header**

1. `app/web/src/api/useBlobUrl.ts`: move the `useBlobUrl` hook out of `AgentsPage.tsx` verbatim (export it); `AgentsPage.tsx` imports it. No behavior change.
2. `ChatPage.tsx`: fetch agents once alongside the models fetch (`client.GET('/v1/agents')` → `agentOptions: AgentOption[]` state + an `agentsById` memo). Change `handleNewChat` to `handleNewChat(agentId: string | null)` and POST `{ body: agentId ? { agent_id: agentId } : {} }`; include `agent_id: agentId ?? undefined` in the optimistic sidebar prepend.
3. `SessionSidebar.tsx`: the New-chat button toggles the `AgentPicker` popover when more than one agent exists (single agent = create immediately, preserving today's one-click flow). Picking calls `onNewChat(agentId)` and closes. Pass `agents` down from ChatPage.
4. Sidebar row chip: in `SessionRow`, when `session.agent_id` is set, replace the `RiChat1Line` glyph with the agent monogram span (same 1-letter circle as the picker, ~`w-4 h-4 text-[0.55rem]`), title = agent name from `agentsById`; unknown id falls back to the plain glyph.
5. Header: next to the title block, when the active session summary has `agent_id`, render the monogram + agent name chip (reuse the picker's row styles; avatar via `useBlobUrl` when the profile has `avatar_blob_id`).
6. `types.ts` `SessionSummary`: add `/** Bound agent profile id; absent = builtin. */ agent_id?: string;`. In ChatPage's list-load and `applySessionPatch` paths, copy `agent_id` through (patch field arrives via the extended `SessionPatch`; update the local wire mirror in `chatWs.ts` if it re-declares the patch shape).
7. `AgentsPage.tsx` skills readout: refetch skills when the selected agent changes — `client.GET('/v1/skills', { params: { query: selected && !selected.builtin ? { agent_id: selected.id } : {} } })`, keeping the initial load for the builtin. (Types come from the regenerated `schema.d.ts`.)

- [ ] **Step 5: Run web gates**

```bash
pnpm --filter baybo-web test && pnpm --filter baybo-web type-check && pnpm --filter baybo-web build
```

Expected: PASS. Manual smoke (optional): `pnpm --filter baybo-web dev` + `?mock=true` on `/agents`.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(web): agent picker on new chat + session agent chip + scoped skills readout"
```

---

### Task 11: Docs + spec sync + full verification gates

**Files:**
- Modify: `docs/modules/agent-profiles.md` (Deferred → shipped consumers)
- Modify: `docs/modules/memory.md` (MemoryContext agent_id; backend partition behavior)
- Modify: `docs/modules/skills.md` (agent overlay + load locations)
- Modify: `docs/modules/workspace.md` + layout tables (agent-skills/)
- Modify: `docs/modules/session.md` (binding columns, if the doc lists session columns)
- Modify: `docs/todo/multi-agent-chat.md` (two Phase-1 amendments)

**Interfaces:** none — documentation + gates.

- [ ] **Step 1: Update module docs**

Describe the CURRENT design directly (house rule: no "used to be" archaeology):

- `agent-profiles.md`: move "Session binding" and "Per-agent skills" out of Deferred into the body — binding = `sessions.agent_id`/`agent_framework` flat columns written once at creation, prompt/llm consumed live, skills overlay from `agent-skills/<id>/`, memory partition key = profile id with `NULL` → builtin. Keep "External-framework top-level sessions" in Deferred (Phase 2).
- `memory.md`: `MemoryContext` carries `agent_id`; mem0 filters + writes by it; OpenViking sends it as `X-OpenViking-Agent`; tools default to the session's agent.
- `skills.md`: add the `agent-skills/<agent_id>/<skill>/SKILL.md` load location, the overlay/collision rule, and that `reload` covers it.
- `workspace.md`: add `agent-skills/` to the layout tree + subsystem table (git-versioned, ensure_layout-created).

- [ ] **Step 2: Amend the spec for two Phase-1 decisions**

In `docs/todo/multi-agent-chat.md`: (a) the live-content table's `llm` row becomes "resolved at actor spawn/hydration — an explicit per-session switch wins immediately; a profile edit lands on the next hydration"; (b) note `external_resume_key` lands with the Phase 2 PR (columns are cheap guarded ALTERs); (c) note the `AgentBinding` struct is realized as `Session.state.{agent_id, agent_framework}` plus a threaded `Arc<dyn AgentProfileStore>` — same data, no separate named type until Phase 2's framework branch needs one.

- [ ] **Step 3: Run the full verification gates**

```bash
cargo fmt --all --check
cargo clippy --all --benches --tests --examples --all-features -- -D warnings
cargo nextest run --workspace
scripts/check-ts-bindings.sh
pnpm install --frozen-lockfile && pnpm -r --if-present run build && pnpm -r --if-present run check && pnpm -r --if-present run test
```

Expected: all green. (Known flake: `crates/tui/tests/chat_render.rs` under load — rerun `--failed` if it trips.)

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "docs: record agent-profile session binding across module docs"
```
