# Agent Model Allow-List + Reasoning Effort Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `AgentProfile` gains an allowed-models set (restricts a bound session's model switching) and a reasoning-effort setting delivered per-request to the LLM layer (consumed by the `openai-subscription` provider in v1).

**Architecture:** Spec: `docs/todo/agent-model-set-effort.md`. Branch: `agent-model-effort` (stacked on `multi_agent` / PR #169). Two new `agent_profiles` columns (JSON-array TEXT + effort TEXT). Effort rides a new `ChatRequest.reasoning_effort` field, bridged across the rig `CompletionRequest` boundary via `additional_params` — gated to the `openai-subscription` model variant so no other provider's API body is polluted. Spawn-time threading replaces `ActorSpawner`'s bare `Option<LlmEntryName>` with a small `SpawnLlmChoice` struct. The allow-list is enforced server-side in `PUT …/model` and mirrored in the web ModelPicker/editor.

**Tech Stack:** Rust workspace (axum/utoipa, libsql, rig), React/TS web app (openapi-typescript, vitest).

## Global Constraints

- No `.unwrap()` / `.expect()` in production code (tests fine). parking_lot only.
- Zero clippy warnings incl. tests: `cargo clippy --all --benches --tests --examples --all-features -- -D warnings`. Test runs never use `--all-features`: `cargo nextest run -p <crate>` / `--workspace`.
- Effort vocabulary EXACTLY `minimal | low | medium | high | xhigh` (the provider's clamp tables also know `"none"`, which stays entry-config-only — the profile enum has NO None variant; profile NULL = follow the entry).
- Empty `allowed_models` = unrestricted. Clearing the pin (`llm: null`) always bypasses the set check.
- Set members validated against live pool entries at write; pin ∈ set when both present; read-time tolerance for entries later removed from `baybo.json`.
- Effort applies to the MAIN loop request only — title generation, progress observer, compression, and every other side-call send `None`.
- Liveness = actor spawn/hydration (same as the llm pin). `SetModel` does not touch effort.
- Migrations: new columns in BOTH the `agent_profiles` CREATE TABLE (`crates/storage/src/libsql/mod.rs:537-548`) AND the guarded ALTER list (~line 611).
- OpenAPI regen after DTO change: `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync` then `pnpm --filter baybo-web gen:api`. No ts-rs wire change in this feature.
- Docs: current-design wording only, no archaeology. `cargo fmt` before every commit.

---

### Task 1: `ReasoningEffort` enum in baybo-model

**Files:**
- Create: `crates/model/src/reasoning_effort.rs`
- Modify: `crates/model/src/lib.rs` (module + re-export, next to `model_tier`)

**Interfaces:**
- Produces: `baybo_model::ReasoningEffort` — `Minimal | Low | Medium | High | XHigh`; `as_str() -> &'static str` (lowercase: `"minimal" | "low" | "medium" | "high" | "xhigh"`); `parse(&str) -> Option<Self>` (ASCII case-insensitive); `pub const ALL: &'static [ReasoningEffort]`; derives `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize` with `#[serde(rename_all = "lowercase")]`.

- [ ] **Step 1: Write the failing test**

In the new file's `#[cfg(test)] mod tests` (mirror `crates/model/src/model_tier.rs`'s test style):

```rust
#[test]
fn reasoning_effort_round_trips_all_variants() {
    for e in ReasoningEffort::ALL {
        assert_eq!(ReasoningEffort::parse(e.as_str()), Some(*e));
        let json = serde_json::to_string(e).unwrap();
        assert_eq!(json, format!("\"{}\"", e.as_str()));
        let back: ReasoningEffort = serde_json::from_str(&json).unwrap();
        assert_eq!(back, *e);
    }
    assert_eq!(ReasoningEffort::parse("XHigh"), Some(ReasoningEffort::XHigh));
    assert_eq!(ReasoningEffort::parse("none"), None);
    assert_eq!(ReasoningEffort::parse(""), None);
    assert_eq!(ReasoningEffort::ALL.len(), 5);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p baybo-model reasoning_effort`
Expected: FAIL (module missing — compile error).

- [ ] **Step 3: Implement**

`crates/model/src/reasoning_effort.rs` (mirror `model_tier.rs`'s shape):

```rust
use serde::{Deserialize, Serialize};

/// Per-agent reasoning-effort request for providers that support it.
/// The profile-level vocabulary; providers clamp to what the concrete
/// model accepts. `"none"` is deliberately absent — disabling reasoning
/// stays an LLM-entry configuration concern, not a persona one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ReasoningEffort {
    pub const ALL: &'static [ReasoningEffort] = &[
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|e| e.as_str().eq_ignore_ascii_case(s.trim()))
    }
}
```

`lib.rs`: `mod reasoning_effort;` + `pub use reasoning_effort::ReasoningEffort;` (match how `ModelTier` is exported).

- [ ] **Step 4: Run tests** — `cargo nextest run -p baybo-model` → PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add -A && git commit -m "feat(model): ReasoningEffort enum"`

---

### Task 2: Storage — profile columns for set + effort

**Files:**
- Modify: `crates/store/src/agent_profile.rs` (row ~24-52)
- Modify: `crates/store/src/test_support.rs` (fake's `update` copies the new fields)
- Modify: `crates/storage/src/libsql/mod.rs` (CREATE TABLE ~537-548; ALTER list tail ~611)
- Modify: `crates/storage/src/libsql/agent_profile.rs` (SELECT_COLS ~16, row_from_libsql ~71-123, create ~169-193, update ~195-216, tests ~261+)

**Interfaces:**
- Consumes: Task 1's `ReasoningEffort`.
- Produces: `AgentProfileRow.allowed_models: Vec<LlmEntryName>` (empty = unrestricted) + `AgentProfileRow.reasoning_effort: Option<ReasoningEffort>`; same two fields on `AgentProfileUpdate`. Storage encodes the set as a JSON string array in a nullable TEXT column (`NULL` ↔ empty Vec both directions — an empty Vec stores NULL); effort as `as_str()` TEXT. Unknown effort string or malformed JSON on read = `StorageError::Storage`.

- [ ] **Step 1: Write the failing test**

In `crates/storage/src/libsql/agent_profile.rs`'s test module (reuse `custom_row` / `content_update` helpers — extend BOTH helpers with the new fields):

```rust
#[tokio::test]
async fn allowed_models_and_effort_round_trip() {
    let store = open_store().await;
    let mut row = custom_row("Tuned");
    row.allowed_models = vec![LlmEntryName::from("fast"), LlmEntryName::from("deep")];
    row.reasoning_effort = Some(baybo_model::ReasoningEffort::High);
    store.create(&row).await.unwrap();

    let back = store.get(&row.id).await.unwrap().unwrap();
    assert_eq!(back.allowed_models, row.allowed_models, "order preserved");
    assert_eq!(back.reasoning_effort, Some(baybo_model::ReasoningEffort::High));

    // Full replace resets both to inherit/unrestricted.
    assert!(store.update(&row.id, &content_update("Tuned 2")).await.unwrap());
    let reset = store.get(&row.id).await.unwrap().unwrap();
    assert!(reset.allowed_models.is_empty());
    assert!(reset.reasoning_effort.is_none());

    // Empty set stores NULL (round-trips as empty, not "[]").
    let plain = custom_row("Plain");
    store.create(&plain).await.unwrap();
    let plain_back = store.get(&plain.id).await.unwrap().unwrap();
    assert!(plain_back.allowed_models.is_empty());
}

#[tokio::test]
async fn corrupt_effort_or_set_column_errors_on_read() {
    let store = open_store().await;
    let row = custom_row("Broken");
    store.create(&row).await.unwrap();
    store
        .pool
        .conn()
        .execute(
            "UPDATE agent_profiles SET reasoning_effort = 'ultra' WHERE id = ?1",
            libsql::params![row.id.as_str().to_string()],
        )
        .await
        .unwrap();
    assert!(store.get(&row.id).await.is_err(), "unknown effort must error");
    store
        .pool
        .conn()
        .execute(
            "UPDATE agent_profiles SET reasoning_effort = NULL, allowed_models = 'not-json' WHERE id = ?1",
            libsql::params![row.id.as_str().to_string()],
        )
        .await
        .unwrap();
    assert!(store.get(&row.id).await.is_err(), "malformed set must error");
}
```

(`custom_row` gets `allowed_models: Vec::new(), reasoning_effort: None,`; if `store.pool` isn't reachable from tests, use whatever raw-conn handle the file's existing tests use — check the legacy-migration test for the pattern.)

- [ ] **Step 2: Run test to verify it fails** — `cargo nextest run -p baybo-storage allowed_models` → FAIL (missing fields).

- [ ] **Step 3: Implement**

1. `crates/store/src/agent_profile.rs` — on `AgentProfileRow` (after `llm`) and `AgentProfileUpdate` (after `llm`):

```rust
    /// LLM entries a bound session may switch to. Empty = unrestricted.
    /// When non-empty and `llm` is also set, the pin is a member
    /// (gateway-enforced at write).
    pub allowed_models: Vec<LlmEntryName>,
    /// Per-request reasoning effort for providers that support it.
    /// `None` = follow the LLM entry's own configured value.
    pub reasoning_effort: Option<ReasoningEffort>,
```

2. `crates/store/src/test_support.rs` — `MemoryAgentProfileStore::update` copies both new fields from the update (alongside the existing five).

3. `crates/storage/src/libsql/mod.rs` — CREATE TABLE gains (after `llm`):

```sql
                    allowed_models  TEXT,
                    reasoning_effort TEXT,
```

and the ALTER list gains:

```rust
            "ALTER TABLE agent_profiles ADD COLUMN allowed_models TEXT",
            "ALTER TABLE agent_profiles ADD COLUMN reasoning_effort TEXT",
```

4. `crates/storage/src/libsql/agent_profile.rs`:

- `SELECT_COLS` appends `, allowed_models, reasoning_effort` (indices 10, 11 — existing indices unchanged).
- `row_from_libsql` (after `updated_at`):

```rust
    let allowed_models_raw: Option<String> = row
        .get(10)
        .map_err(|e| col_err("agent_profiles.allowed_models", e))?;
    let allowed_models: Vec<LlmEntryName> = match allowed_models_raw {
        None => Vec::new(),
        Some(json) => {
            let names: Vec<String> = serde_json::from_str(&json).map_err(|e| {
                StorageError::Storage(format!("agent_profiles.allowed_models: bad JSON: {e}"))
            })?;
            names.into_iter().map(LlmEntryName::from).collect()
        }
    };
    let reasoning_effort_raw: Option<String> = row
        .get(11)
        .map_err(|e| col_err("agent_profiles.reasoning_effort", e))?;
    let reasoning_effort = match reasoning_effort_raw {
        None => None,
        Some(s) => Some(ReasoningEffort::parse(&s).ok_or_else(|| {
            StorageError::Storage(format!(
                "agent_profiles.reasoning_effort: unknown value {s:?}"
            ))
        })?),
    };
```

- One private encode helper used by `create` AND `update`:

```rust
fn encode_allowed_models(models: &[LlmEntryName]) -> Result<Option<String>> {
    if models.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = models.iter().map(LlmEntryName::as_str).collect();
    serde_json::to_string(&names)
        .map(Some)
        .map_err(|e| StorageError::Storage(format!("encode allowed_models: {e}")))
}
```

- `create`: INSERT column list gains `allowed_models, reasoning_effort` (`?10, ?11`); params add `encode_allowed_models(&row.allowed_models)?` and `row.reasoning_effort.map(|e| e.as_str().to_owned())`.
- `update`: SET clause becomes `name = ?2, description = ?3, system_prompt = ?4, framework = ?5, llm = ?6, allowed_models = ?7, reasoning_effort = ?8, updated_at = ?9` (i.e. the two new columns slot before `updated_at`, whose binding moves to `?9`); params likewise from the update struct.

5. Compile-fix every `AgentProfileRow {` / `AgentProfileUpdate {` literal the compiler flags (context tests' `profile_row` helper, router test helper, gateway handlers get real values in Task 5 — until then `allowed_models: Vec::new(), reasoning_effort: None`).

- [ ] **Step 4: Run tests** — `cargo nextest run -p baybo-storage -p baybo-store && cargo build --workspace` → PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add -A && git commit -m "feat(storage): agent_profiles allowed_models + reasoning_effort columns"`

---

### Task 3: LLM layer — `ChatRequest.reasoning_effort` + gated bridge + provider override

**Files:**
- Modify: `crates/llm/src/lib.rs` (ChatRequest ~229-235; `build_completion_request` ~733; `AnyCompletionModel` impl ~502; the chat/chat_stream dispatch ~686/715)
- Modify: `crates/llm/src/providers/openai_subscription/completion_model.rs` (`stream` ~203, `completion` ~194)
- Modify (compile-fix `reasoning_effort: None`): all 20 other `ChatRequest {` literals — `command rg -n 'ChatRequest \{' crates/ --type rust` (skills-assessor ×2, context ×2, agent ×3, tools ×2, llm tests/test_support/guard/billed, integration-tests)

**Interfaces:**
- Consumes: nothing new (plain strings at this layer — the enum stays above).
- Produces:
  - `ChatRequest.reasoning_effort: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, beside `temperature`).
  - `pub(crate) const REQUEST_REASONING_EFFORT_PARAM: &str = "baybo_reasoning_effort";` in `crates/llm/src/lib.rs`.
  - Bridge: in `LlmClient::chat` AND `chat_stream`, after `build_completion_request`, when `request.reasoning_effort` is Some AND the model variant accepts it, set `rig_request.additional_params = Some(json!({ REQUEST_REASONING_EFFORT_PARAM: effort }))`.
  - `AnyCompletionModel::accepts_request_reasoning_effort(&self) -> bool` — `matches!(self, Self::OpenAiSubscription(_))`.
  - Provider: `stream()` (and `completion()` if it doesn't delegate to `stream`) extracts the param, resolves `super::reasoning::resolve_effort(&self.model, Some(v))`, uses it in place of `self.reasoning_effort`, `warn!`s when the resolved value differs from the requested one (clamp visibility).

- [ ] **Step 1: Write the failing provider test**

In `completion_model.rs`'s test module, next to `body_emits_reasoning_when_effort_is_set` (~line 1360). The extraction helper is the unit to test — implement it as a small function so the test doesn't need a live client:

```rust
#[test]
fn request_effort_overrides_baked_value() {
    // Baked "low", request asks "high" → high wins (legal for gpt-5).
    let mut req = empty_request();
    req.additional_params =
        Some(serde_json::json!({ crate::REQUEST_REASONING_EFFORT_PARAM: "high" }));
    assert_eq!(
        effective_reasoning_effort("gpt-5", Some("low"), &req),
        Some("high")
    );
    // No request value → baked value.
    let plain = empty_request();
    assert_eq!(
        effective_reasoning_effort("gpt-5", Some("low"), &plain),
        Some("low")
    );
    // Out-of-range request clamps per the model family (gpt-5-pro is high-only).
    let mut clamped = empty_request();
    clamped.additional_params =
        Some(serde_json::json!({ crate::REQUEST_REASONING_EFFORT_PARAM: "minimal" }));
    assert_eq!(
        effective_reasoning_effort("gpt-5-pro", Some("high"), &clamped),
        Some("high")
    );
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo nextest run -p baybo-llm request_effort_overrides` → FAIL.

- [ ] **Step 3: Implement**

1. `ChatRequest` field + const (lib.rs):

```rust
    /// Per-request reasoning-effort ask (profile-driven). Providers that
    /// support it clamp to the concrete model's range; all others ignore
    /// it. `None` = the provider's own configured behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
```

```rust
/// Key under rig's `additional_params` carrying the per-request effort
/// across the `CompletionRequest` boundary. Namespaced so it can never be
/// mistaken for a provider API parameter; only set when the target model
/// variant is known to read it.
pub(crate) const REQUEST_REASONING_EFFORT_PARAM: &str = "baybo_reasoning_effort";
```

2. Bridge in `LlmClient::chat` and `chat_stream` (identical few lines at both sites, right after `let rig_request = self.build_completion_request(request).await;` — make `rig_request` `mut`):

```rust
        let mut rig_request = self.build_completion_request(request).await;
        if let Some(effort) = &request.reasoning_effort
            && self.model.accepts_request_reasoning_effort()
        {
            rig_request.additional_params =
                Some(serde_json::json!({ REQUEST_REASONING_EFFORT_PARAM: effort }));
        }
```

(If `additional_params` is already populated somewhere in the future, this overwrites — today `build_completion_request` always leaves it `None`; add a one-line comment noting the assumption.)

3. `AnyCompletionModel` inherent method:

```rust
    /// Whether this variant reads [`REQUEST_REASONING_EFFORT_PARAM`] from
    /// `additional_params`. Gating the bridge here keeps the marker out of
    /// every other provider's request payload.
    pub(crate) fn accepts_request_reasoning_effort(&self) -> bool {
        matches!(self, Self::OpenAiSubscription(_))
    }
```

4. Provider (`completion_model.rs`) — a free helper + use at the body-build site(s):

```rust
/// The effort for THIS request: an explicit per-request ask (clamped to the
/// model's range, with a warn when clamping changed it) beats the
/// construction-baked value.
fn effective_reasoning_effort(
    model: &str,
    baked: Option<&'static str>,
    request: &CompletionRequest,
) -> Option<&'static str> {
    let requested = request
        .additional_params
        .as_ref()
        .and_then(|p| p.get(crate::REQUEST_REASONING_EFFORT_PARAM))
        .and_then(|v| v.as_str());
    match requested {
        None => baked,
        Some(ask) => {
            let resolved = super::reasoning::resolve_effort(model, Some(ask));
            let clamped = match resolved {
                Some(r) => !r.eq_ignore_ascii_case(ask),
                None => true, // "none" outcomes can't come from the profile vocab
            };
            if clamped {
                tracing::warn!(
                    model = %model,
                    requested = %ask,
                    resolved = ?resolved,
                    "per-request reasoning effort clamped to the model's range"
                );
            }
            resolved
        }
    }
}
```

In `stream()` replace `build_responses_body(&self.model, self.reasoning_effort, &request)` with:

```rust
        let effort = effective_reasoning_effort(&self.model, self.reasoning_effort, &request);
        let body = build_responses_body(&self.model, effort, &request).map_err(|msg| {
```

(Check `completion()` at ~line 194: if it delegates to `stream()`, one site suffices; if it builds its own body, apply the same two lines there.)

5. Compile-fix all other `ChatRequest {` literals with `reasoning_effort: None` — the side-call sites (`title.rs:48`, `agent_loop.rs:2466`, `compressor.rs:475`, `background_summary.rs:588`, `judge.rs:178`, `web_fetch.rs:593`, skills-assessor ×2) are DELIBERATELY `None` per the spec; the main-loop site gets the real value in Task 4.

- [ ] **Step 4: Run tests** — `cargo nextest run -p baybo-llm && cargo build --workspace` → PASS.
- [ ] **Step 5: Commit** — `cargo fmt && git add -A && git commit -m "feat(llm): per-request reasoning effort, consumed by openai-subscription"`

---

### Task 4: Agent runtime — `SpawnLlmChoice` threading + main-loop fill

**Files:**
- Modify: `crates/agent/src/actor/router/mod.rs` (ActorSpawner ~126-136, build_oneshot_actor ~145-156, spawn_oneshot_actor ~290-304)
- Modify: `crates/agent/src/actor/router/user_input.rs` (resolve_initial_llm ~28-50 → resolve_spawn_llm; call sites ~176, ~347; test ~729-759)
- Modify: `crates/agent/src/actor/router/cron.rs` (~57)
- Modify: `crates/agent/src/runtime/subagent_spawner.rs` (~311-317)
- Modify: `crates/agent/src/runtime/agent_loop.rs` (AgentLoopConfig ~460-484, from_config, call_llm ~1459)
- Modify: `crates/baybo/src/runtime.rs` (factory closure ~815-834)
- Modify: `crates/integration-tests/src/harness.rs` (config literal ~503; add a builder knob)

**Interfaces:**
- Consumes: Task 1's `ReasoningEffort`; Task 2's profile fields; Task 3's `ChatRequest.reasoning_effort`.
- Produces:
  - `pub struct SpawnLlmChoice { pub initial_llm: Option<LlmEntryName>, pub reasoning_effort: Option<ReasoningEffort> }` (`#[derive(Debug, Clone, Default)]`, in `router/mod.rs` next to `ActorSpawner`).
  - `ActorSpawner`'s second parameter becomes `SpawnLlmChoice`.
  - `resolve_spawn_llm(store, session) -> SpawnLlmChoice` replacing `resolve_initial_llm`: ONE profile fetch when bound (even when `last_llm` is set — effort still comes from the profile); `initial_llm = last_llm.or(profile.llm)`; `reasoning_effort = profile.reasoning_effort`; unbound → `SpawnLlmChoice::default()` with no fetch; missing profile/store error → warn + defaults.
  - `AgentLoopConfig.reasoning_effort: Option<ReasoningEffort>`; `call_llm` fills `reasoning_effort: self.reasoning_effort.map(|e| e.as_str().to_owned())` in the main-loop `ChatRequest` (site agent_loop.rs:1459 ONLY — the 2466 side-call stays `None`).
  - Harness builder gains `with_reasoning_effort(ReasoningEffort)` feeding the config.

- [ ] **Step 1: Reshape the failing router test**

Rewrite `initial_llm_prefers_session_pin_then_profile_pin` (user_input.rs:729) as `spawn_llm_resolves_pin_and_effort` — extend the existing `profile_row` test helper with `reasoning_effort: Some(ReasoningEffort::High)` on A1 (and the two new Vec/None fields):

```rust
    #[tokio::test]
    async fn spawn_llm_resolves_pin_and_effort() {
        use baybo_store::test_support::MemoryAgentProfileStore;
        let store = MemoryAgentProfileStore::new();
        store.insert(profile_row("A1", Some(LlmEntryName::from("profile-pin"))));
        let store: Arc<dyn baybo_store::agent_profile::AgentProfileStore> = store;

        let mut session = make_session();

        // Unbound → all defaults, no fetch.
        let choice = resolve_spawn_llm(&store, &session).await;
        assert_eq!(choice.initial_llm, None);
        assert_eq!(choice.reasoning_effort, None);

        // Bound, no explicit pin → profile pin + profile effort.
        session.state.agent_id = Some(baybo_model::AgentProfileId::from("A1"));
        let choice = resolve_spawn_llm(&store, &session).await;
        assert_eq!(choice.initial_llm, Some(LlmEntryName::from("profile-pin")));
        assert_eq!(choice.reasoning_effort, Some(baybo_model::ReasoningEffort::High));

        // Explicit session pin wins for the MODEL, effort still applies.
        session.state.last_llm = Some(LlmEntryName::from("user-pick"));
        let choice = resolve_spawn_llm(&store, &session).await;
        assert_eq!(choice.initial_llm, Some(LlmEntryName::from("user-pick")));
        assert_eq!(choice.reasoning_effort, Some(baybo_model::ReasoningEffort::High));

        // Deleted profile degrades to pin-only defaults.
        session.state.agent_id = Some(baybo_model::AgentProfileId::from("GONE"));
        let choice = resolve_spawn_llm(&store, &session).await;
        assert_eq!(choice.initial_llm, Some(LlmEntryName::from("user-pick")));
        assert_eq!(choice.reasoning_effort, None);
    }
```

- [ ] **Step 2: Run test to verify it fails** — `cargo nextest run -p baybo-agent spawn_llm_resolves` → FAIL.

- [ ] **Step 3: Implement**

`router/mod.rs`:

```rust
/// LLM parameters resolved at actor spawn: the effective model pin and the
/// bound agent profile's reasoning effort. Cheap to clone; `Default` = pool
/// default model, provider-configured effort.
#[derive(Debug, Clone, Default)]
pub struct SpawnLlmChoice {
    pub initial_llm: Option<LlmEntryName>,
    pub reasoning_effort: Option<baybo_model::ReasoningEffort>,
}
```

`ActorSpawner`'s second param: `/* llm_choice */ SpawnLlmChoice`. `build_oneshot_actor` / `spawn_oneshot_actor` take `llm_choice: SpawnLlmChoice`.

`user_input.rs`:

```rust
pub(crate) async fn resolve_spawn_llm(
    store: &Arc<dyn AgentProfileStore>,
    session: &Session,
) -> SpawnLlmChoice {
    let Some(agent_id) = session.state.agent_id.as_ref() else {
        return SpawnLlmChoice {
            initial_llm: session.state.last_llm.clone(),
            ..Default::default()
        };
    };
    let profile = match store.get(agent_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => {
            return SpawnLlmChoice {
                initial_llm: session.state.last_llm.clone(),
                ..Default::default()
            };
        }
        Err(e) => {
            warn!(
                agent_id = %agent_id, error = %e,
                "agent profile lookup failed at spawn; using defaults"
            );
            return SpawnLlmChoice {
                initial_llm: session.state.last_llm.clone(),
                ..Default::default()
            };
        }
    };
    SpawnLlmChoice {
        initial_llm: session.state.last_llm.clone().or(profile.llm),
        reasoning_effort: profile.reasoning_effort,
    }
}
```

Both call sites: `let llm_choice = resolve_spawn_llm(&self.agent_profile_store, &session).await;` → closure passes `llm_choice`. Cron: `self.spawn_oneshot_actor(session, SpawnLlmChoice::default(), response_tx, &self.actor_parent_token)`. Subagent spawner: `SpawnLlmChoice { initial_llm: llm, ..Default::default() }`.

`agent_loop.rs`: `AgentLoopConfig.reasoning_effort: Option<ReasoningEffort>` (doc: "Per-agent reasoning effort for the MAIN loop's LLM calls; side-calls stay provider-default"); `from_config` stores it on the loop; `call_llm`'s request:

```rust
        let request = ChatRequest {
            messages: self.context_manager.messages_for_llm(),
            temperature: None,
            reasoning_effort: self.reasoning_effort.map(|e| e.as_str().to_owned()),
            tools: tool_defs,
        };
```

`crates/baybo/src/runtime.rs` factory: closure header takes `llm_choice: baybo_agent::router::SpawnLlmChoice`; config gets `initial_llm: llm_choice.initial_llm, reasoning_effort: llm_choice.reasoning_effort,`.

Harness (`integration-tests/src/harness.rs`): config literal gains `reasoning_effort: self.reasoning_effort,`; builder field + `pub fn with_reasoning_effort(mut self, e: ReasoningEffort) -> Self` (default `None`).

- [ ] **Step 4: Add the e2e**

In `crates/integration-tests/tests/agent_loop_e2e.rs` (mirror an existing captured_requests test, e.g. ~461):

```rust
#[tokio::test]
async fn main_loop_requests_carry_configured_reasoning_effort() {
    let mut harness = AgentTestHarness::builder()
        .with_reasoning_effort(baybo_model::ReasoningEffort::High)
        .build();
    harness.stub_llm.push_stream(vec![StreamEvent::Text("ok".into())]);
    harness.send_text("hello").await.unwrap();
    harness.drain_outputs(DRAIN_TIMEOUT).await;
    let reqs = harness.stub_llm.captured_requests();
    assert!(!reqs.is_empty());
    assert_eq!(reqs[0].reasoning_effort.as_deref(), Some("high"));

    let mut plain = AgentTestHarness::builder().build();
    plain.stub_llm.push_stream(vec![StreamEvent::Text("ok".into())]);
    plain.send_text("hello").await.unwrap();
    plain.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(plain.stub_llm.captured_requests()[0].reasoning_effort, None);
}
```

- [ ] **Step 5: Run tests** — `cargo nextest run -p baybo-agent -p baybo-integration-tests && cargo nextest run --workspace` → PASS.
- [ ] **Step 6: Commit** — `cargo fmt && git add -A && git commit -m "feat(agent): thread per-agent reasoning effort to main-loop LLM requests"`

---

### Task 5: Gateway — profile validation + DTOs + model-switch enforcement

**Files:**
- Modify: `crates/gateway/src/api/admin/agents.rs` (DTOs ~69-139, validators ~152-199, create_agent ~250-281, update_agent ~315-342)
- Modify: `crates/gateway/src/api/admin/chat.rs` (set_session_model ~1176-1219)
- Modify: `crates/gateway/tests/agents_api.rs`, `crates/gateway/tests/chat_api.rs`
- Regen: `docs/openapi.json`, `app/web/src/api/schema.d.ts`

**Interfaces:**
- Consumes: Tasks 1-2.
- Produces:
  - `AgentProfileDto` gains `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub allowed_models: Vec<String>` + `#[serde(default, skip_serializing_if = "Option::is_none")] pub reasoning_effort: Option<String>` (From impl maps `r.allowed_models.iter().map(|n| n.to_string()).collect()` / `r.reasoning_effort.map(|e| e.as_str().to_owned())`).
  - `CreateAgentProfileRequest` / `UpdateAgentProfileRequest` gain `#[serde(default)] pub allowed_models: Vec<String>` + `#[serde(default)] pub reasoning_effort: Option<String>`.
  - Two validators in `agents.rs`:

```rust
/// Validate + normalize the allowed-models set: every member must be a live
/// pool entry; duplicates collapse (order preserved); when a pin is also
/// set, it must be a member.
fn validate_allowed_models(
    state: &AdminState,
    raw: &[String],
    pin: Option<&LlmEntryName>,
) -> Result<Vec<LlmEntryName>> {
    let mut out: Vec<LlmEntryName> = Vec::with_capacity(raw.len());
    for name in raw {
        let entry = super::validate_llm_pin(state, Some(name))?.ok_or_else(|| {
            GatewayError::BadRequest("allowed_models entries must not be empty".to_owned())
        })?;
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    if let (Some(pin), false) = (pin, out.is_empty())
        && !out.contains(pin)
    {
        return Err(GatewayError::BadRequest(format!(
            "llm pin {:?} is not in allowed_models",
            pin.as_str()
        )));
    }
    Ok(out)
}

fn validate_reasoning_effort(raw: Option<&str>) -> Result<Option<ReasoningEffort>> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => match ReasoningEffort::parse(s) {
            Some(e) => Ok(Some(e)),
            None => Err(GatewayError::BadRequest(format!(
                "unknown reasoning_effort {s:?}; expected one of {}",
                ReasoningEffort::ALL
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        },
    }
}
```
  - `create_agent` / `update_agent` call both after `validate_llm_pin` and put the results in the row/update.
  - `set_session_model` gains set enforcement after `validate_llm_pin` (the handler already has the loaded `session` from `load_scoped_chat_session` — change the `(sid, _)` destructure to `(sid, session)`):

```rust
    if let (Some(pin), Some(agent_id)) = (pin.as_ref(), session.state.agent_id.as_ref()) {
        match state.agent_profile_store.get(agent_id).await {
            Ok(Some(profile))
                if !profile.allowed_models.is_empty()
                    && !profile.allowed_models.contains(pin) =>
            {
                return Err(GatewayError::BadRequest(format!(
                    "model {:?} is not in agent {:?}'s allowed set",
                    pin.as_str(),
                    profile.name
                )));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(GatewayError::Internal(format!(
                    "load agent profile for model-set check: {e}"
                )));
            }
        }
    }
```

- [ ] **Step 1: Write the failing gateway tests**

`agents_api.rs` — a new test following `agents_api_round_trip`'s helper style (`post_expect`/`put_expect`/`get`; the pool's single entry name comes from `tg.deps.llm_pool.read().entry_names().first()`):

```rust
#[tokio::test]
async fn agent_model_set_and_effort_validation() {
    // setup identical to agents_api_round_trip
    // 1. POST with allowed_models [valid_name], reasoning_effort "high",
    //    llm valid_name → 200; GET echoes both (allowed_models == [valid_name],
    //    reasoning_effort == "high").
    // 2. POST with allowed_models ["nope"] → 400.
    // 3. POST with llm valid_name + allowed_models NOT containing it →
    //    can't build with a one-entry pool — instead: POST with
    //    allowed_models [valid_name] + llm absent → 200; then PUT llm to a
    //    bogus name → 400 (pin unknown) — and separately assert the
    //    pin-not-in-set rule via a duplicate of the valid entry? With a
    //    single-entry pool the mismatch case needs a second entry: SKIP the
    //    mismatch case here and cover pin∈set in the unit test below.
    // 4. POST with reasoning_effort "ultra" → 400 mentioning "minimal".
    // 5. PUT full-replace without the two fields → GET shows both reset
    //    (allowed_models absent/empty, reasoning_effort absent).
    // 6. Duplicates collapse: POST allowed_models [valid, valid] → GET
    //    shows one entry.
}
```

Realize each numbered point as real request/assert code. For the pin-∈-set mismatch (needs two entries), add a focused unit test for `validate_allowed_models` in `agents.rs`'s `#[cfg(test)]` if `AdminState` can't be built cheaply there — otherwise check how `build_test_deps` seeds the pool (`crates/gateway/src/test_support.rs`): if it accepts multiple entries, use two and cover the mismatch in the integration test directly.

`chat_api.rs` — extend `set_session_model_validates_persists_and_clears` or add a sibling:

```rust
#[tokio::test]
async fn set_session_model_enforces_agent_allowed_set() {
    // 1. Create profile with allowed_models [<other-than-valid>] — with a
    //    single-entry pool this means allowed_models [valid_name]; create a
    //    session bound to it; PUT model valid_name → 200 (member ok).
    // 2. Create a second profile with allowed_models [valid_name]; bind a
    //    session; PUT {"llm": null} → 200 (clear always bypasses).
    // 3. Unbound session: PUT valid_name → 200 (no set, no restriction).
    // 4. The 400 path needs a name outside the set that still passes
    //    validate_llm_pin — impossible with a one-entry pool, so assert it
    //    at unit level unless test_support seeds ≥2 entries (check first;
    //    if it does, cover the 400 here end-to-end).
}
```

(The implementer MUST check `build_test_deps`'s pool seeding first and prefer end-to-end coverage of the 400 path when two entries are available.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo nextest run -p baybo-gateway agent_model_set` → FAIL (unknown fields ignored → echo asserts fail).
- [ ] **Step 3: Implement** everything in Interfaces above.
- [ ] **Step 4: Regenerate + run**

```bash
UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync
pnpm --filter baybo-web gen:api
cargo nextest run -p baybo-gateway
```

Expected: regen diffs committed; tests PASS.

- [ ] **Step 5: Commit** — `cargo fmt && git add -A && git commit -m "feat(gateway): agent allowed-models + reasoning-effort validation and enforcement"`

---

### Task 6: Web — editor controls + ModelPicker filtering

**Files:**
- Modify: `app/web/src/pages/AgentsPage.tsx` (state ~484-503, save ~539-591, llm SelectBox ~730-752)
- Modify: `app/web/src/pages/ChatPage.tsx` (AgentEntry ~184-186 + projection ~956-966, ModelPicker mount ~2958-2965, ModelPicker component ~5308+)
- Test: extend `app/web/src/pages/chat/AgentPicker.test.tsx` conventions with a new `ModelPicker` filter test only if ModelPicker is exported; otherwise cover filtering via a pure helper function + unit test (see Step 3.4)

**Interfaces:**
- Consumes: Task 5's regenerated `schema.d.ts` (`AgentProfileDto.allowed_models?: string[]`, `reasoning_effort?: string | null`).
- Produces: editor round-trips both fields; bound sessions' ModelPicker filters to the set.

- [ ] **Step 1: Editor state + save**

`AgentsPage.tsx` `AgentEditorPanel`:

```tsx
  const [allowedModels, setAllowedModels] = useState<string[]>(agent?.allowed_models ?? []);
  const [reasoningEffort, setReasoningEffort] = useState(agent?.reasoning_effort ?? '');
```

Save `content` object gains:

```tsx
        allowed_models: allowedModels,
        reasoning_effort: reasoningEffort === '' ? null : reasoningEffort,
```

- [ ] **Step 2: Editor controls**

Below the existing Model select (same `flex` row or a sibling row, matching field styles):

1. **Effort select** — a `SelectBox` mirroring the llm pin's disabled/greyed handling:

```tsx
            <div className="flex-1" title={externalFramework ? BAYBO_ONLY_HINT : undefined}>
              <label className={fieldLabel}>
                Reasoning effort {externalFramework && <span className="normal-case">(baybo only)</span>}
              </label>
              <SelectBox
                className={`w-full h-10 !border ${contentLocked || externalFramework ? 'opacity-60' : ''}`}
                value={reasoningEffort}
                disabled={contentLocked || externalFramework}
                onChange={(e) => setReasoningEffort(e.target.value)}
              >
                <option value="">Default (entry setting)</option>
                {['minimal', 'low', 'medium', 'high', 'xhigh'].map((e) => (
                  <option key={e} value={e}>
                    {e}
                  </option>
                ))}
              </SelectBox>
            </div>
```

2. **Allowed-models checkbox list** (net-new pattern — keep it minimal and brutal-styled):

```tsx
            <div title={externalFramework ? BAYBO_ONLY_HINT : undefined}>
              <label className={fieldLabel}>
                Allowed models {externalFramework && <span className="normal-case">(baybo only)</span>}
              </label>
              <p className="text-[0.7rem] text-ink-soft mb-1">
                None checked = every configured model. Checked entries are the
                only models sessions bound to this agent may switch to.
              </p>
              <div
                className={`border-2 border-black rounded-md p-2 flex flex-col gap-1 max-h-40 overflow-y-auto ${
                  contentLocked || externalFramework ? 'opacity-60 pointer-events-none' : ''
                }`}
              >
                {llmNames.map((n) => (
                  <label key={n} className="flex items-center gap-2 text-sm cursor-pointer">
                    <input
                      type="checkbox"
                      checked={allowedModels.includes(n)}
                      disabled={contentLocked || externalFramework}
                      onChange={(e) =>
                        setAllowedModels((prev) =>
                          e.target.checked ? [...prev, n] : prev.filter((m) => m !== n),
                        )
                      }
                    />
                    <span className="truncate">{n}</span>
                  </label>
                ))}
                {allowedModels
                  .filter((m) => !llmNames.includes(m))
                  .map((m) => (
                    <label key={m} className="flex items-center gap-2 text-sm opacity-70">
                      <input
                        type="checkbox"
                        checked
                        disabled={contentLocked || externalFramework}
                        onChange={() =>
                          setAllowedModels((prev) => prev.filter((x) => x !== m))
                        }
                      />
                      <span className="truncate">{m} (unavailable)</span>
                    </label>
                  ))}
              </div>
            </div>
```

3. **Pin select filtered to the set**: in the existing Model `SelectBox`, replace `llmNames.map(...)` with `(allowedModels.length > 0 ? llmNames.filter((n) => allowedModels.includes(n)) : llmNames).map(...)` — the stale-pin `(unavailable)` fallback branch stays.

- [ ] **Step 3: ModelPicker filtering**

1. `AgentEntry` gains `allowedModels: string[]`; the projection adds `allowedModels: a.allowed_models ?? []`.
2. At the mount site compute:

```tsx
                {sessionId && models.length > 1 ? (
                  <ModelPicker
                    models={visibleModels}
                    defaultName={defaultModelName}
                    current={currentView.model}
                    onSelect={handleSelectModel}
                  />
                ) : null}
```

with, near `activeSession`:

```tsx
  const activeAgent = activeSession?.agent_id
    ? agentsById.get(activeSession.agent_id)
    : undefined;
  const visibleModels = useMemo(
    () => filterAllowedModels(models, activeAgent?.allowedModels ?? []),
    [models, activeAgent],
  );
```

3. Out-of-set current flag inside `ModelPicker`: before the `models.map(...)` rows, when `pinned !== null && !models.some((m) => m.name === pinned)` render one extra non-clickable row `label={pinned}` `sublabel="(not in allowed set)"` `selected` (reuse `ModelPickerRow` with `onClick={() => {}}` or a disabled variant matching its props).
4. Extract the filter into `app/web/src/pages/chat/modelFilter.ts`:

```ts
export function filterAllowedModels<T extends { name: string }>(
  models: T[],
  allowed: string[],
): T[] {
  if (allowed.length === 0) return models;
  return models.filter((m) => allowed.includes(m.name));
}
```

use it at the mount site, and add `modelFilter.test.ts` (vitest, no DOM):

```ts
import { describe, expect, it } from 'vitest';
import { filterAllowedModels } from './modelFilter';

describe('filterAllowedModels', () => {
  const models = [{ name: 'a' }, { name: 'b' }, { name: 'c' }];
  it('empty set = unrestricted', () => {
    expect(filterAllowedModels(models, [])).toHaveLength(3);
  });
  it('filters to members, preserving model order', () => {
    expect(filterAllowedModels(models, ['c', 'a']).map((m) => m.name)).toEqual(['a', 'c']);
  });
  it('unknown members filter everything they do not match', () => {
    expect(filterAllowedModels(models, ['zzz'])).toHaveLength(0);
  });
});
```

- [ ] **Step 4: Run web gates** — `pnpm --filter baybo-web test && pnpm --filter baybo-web type-check && pnpm --filter baybo-web build` → PASS.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(web): agent allowed-models + reasoning-effort editor and picker filtering"`

---

### Task 7: Docs + full gates

**Files:**
- Modify: `docs/modules/agent-profiles.md` (data model + session-binding consumption + editor description)
- Modify: `docs/modules/llm.md` (ChatRequest field + provider consumption matrix)
- Modify: `docs/modules/agent.md` (SpawnLlmChoice, if the doc describes ActorSpawner/spawn resolution)
- Modify: `docs/todo/agent-model-set-effort.md` (mark shipped scope if wording drifted)

**Interfaces:** none — docs + verification.

- [ ] **Step 1: Update docs** (current-design wording; cover: the two profile fields + NULL semantics, write-validation rules, switch-time enforcement + clear-pin bypass, read-time tolerance, `ChatRequest.reasoning_effort` + the `additional_params` bridge being gated to `openai-subscription`, main-loop-only scope, spawn-time liveness).
- [ ] **Step 2: Full gates**

```bash
cargo fmt --all --check
cargo clippy --all --benches --tests --examples --all-features -- -D warnings
cargo nextest run --workspace
scripts/check-ts-bindings.sh
pnpm install --frozen-lockfile && pnpm -r --if-present run build && pnpm -r --if-present run check && pnpm -r --if-present run test
```

Expected: all green.

- [ ] **Step 3: Commit** — `git add -A && git commit -m "docs: record agent model allow-list + reasoning effort"`
