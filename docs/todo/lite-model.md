# Lite model for auxiliary LLM calls — implementation plan

**Status:** PR 1 (`model_list`) merged as `3ef4c4cd`. PR 2 (lite) implemented on
`feat/lite-model`. Kept as the record of why each decision was made; §7 lists the three places the
implementation diverged from the plan. The surviving design lives in
[`docs/modules/config.md`](../modules/config.md) §"LLM entries and `model_list`".

PR 2 could not ship first: its WebFetch input clamp reads a per-model `context_window`, which only
PR 1 makes correct.

Today every auxiliary LLM call in the process runs on the **session's active chat model**: the
Bash risk judges, WebFetch's page summariser, and title generation all bind the same
`AgentLoop::llm_client`. This plan routes four of them to a cheaper *lite* model, resolved from a
cascade the operator configures, while leaving the calls whose input is the shared transcript
prefix on the main model.

The `LlmEntry.lite_model` config field already exists (`crates/config/src/llm.rs:31`), documented
as "Reserved — no runtime path consumes it yet". PR 2 is what consumes it.

---

## 1. Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Scope is **swapping the model behind existing LLM-mediated calls**, not widening where an LLM mediates. Deterministic approval gates (Write/Edit sensitive paths, WebFetch host shape, the executor's access-declaration check) stay deterministic. | Turning a deterministic gate into model discretion is an independent and much larger product change; it is orthogonal to which model runs. |
| D2 | Four consumers go lite: `judge_pre_exec`, `judge_post_fail`, WebFetch summary, title generation. | They are the calls whose input is **standalone** — a command line, 2 KiB of captured output, a fetched page, one user message. |
| D3 | The progress observer, context compression, and the skill assessor **stay on the main model**. | Observer and compression send the session transcript, which is exactly the prefix Anthropic prompt-caching keeps warm (`anthropic.rs:60` `with_prompt_caching()`; `agent_loop.rs:2655` reuses `messages_for_llm()` verbatim *to get the cache hit*). Anthropic's cache is keyed per model, so switching models turns every fire into a cold full-transcript read plus a cache write — on a Sonnet main model that is a net **loss**. The skill assessor caches verdicts by content hash and binds once at boot, so its call volume — and therefore the saving — is ~0, while its verdict sets a skill's trust level. |
| D4 | **The rule, stated once for future call sites:** an auxiliary call may go lite only if its input is *not* the session transcript prefix. | Generalises D2/D3 so the next auxiliary call doesn't have to re-derive it. |
| D5 | Resolution cascade: `entry.lite_model` → `agent.model_tiers[Lite]` → **the session's current client**. Most specific wins. | Per-entry lite is "the cheap slot inside this entry" (same provider, same credentials, so the user's data does not move providers); the tier is the global fallback. Reversing the order would make a configured `lite_model` inert. |
| D6 | `resolve_lite` returns `Arc<BillableLlm>`, **never `Option`**, and owns the terminal fallback. | The judges are fail-closed: no LLM ⇒ `PreExec::Prompt` (`judge.rs:116`). A `None` for unconfigured deployments would silently turn "auto-judge every destructive command" into "prompt on every destructive command" — a pure regression on the **default** `PermissionPolicy::Auto`. Keeping the fallback inside the pool means no call site can forget it. |
| D7 | No double hop: if the tier's target entry has its own `lite_model`, use that entry's **default** model. | Two-level indirection is not debuggable from a config file, and "point the tier at a cheap entry" already expresses the operator's intent. |
| D8 | `ModelTier::Fast` is **renamed** to `Lite`, canonical label `"lite"`, with `#[serde(alias = "fast")]` and `parse()` still accepting `"fast"`. | One concept, one name. The alias is mandatory, not cosmetic: `agent.model_tiers` is a `HashMap<ModelTier, _>` whose **keys** deserialize through the enum, so an un-aliased rename turns every existing `baybo.json` into a hard config-load failure. |
| D9 | `ToolContext.llm` is renamed `lite_llm` and stays a **single** slot. | After D2 every consumer of that slot is a lite consumer. A second slot would be an `Option` field with no reader. The rename is the point: a future tool author reading `ctx.llm` would otherwise assume the session's main model and silently get a weak one. |
| D10 | `judge_post_fail` runs on lite too, and a lite `safe` verdict still reaches `PostFail::Unsandbox` — no strong-model confirmation, no mechanical downgrade. | Owner's call, made against the recommendation recorded in §6. |
| D11 | lite models are **not** added to `entry_models`, so they are not pinnable from the chat model picker. | Two non-overlapping sentences: `model_list` is what a user may pick, `lite_model` is what the runtime picks for itself. An operator who wants it in the picker lists it in `model_list` as well — it is then built once and serves both roles. |
| D12 | A surviving entry whose declared `lite_model` fails to **build** aborts boot and rejects a reload. If the entry itself was dropped (its own default model failed to build), its lite is not chased. | Client construction is synchronous and offline (`registry.rs:314 create_client`, `LlmProviderFactory::create`; the only `await` in the build path is the vault read), so a build failure is always a deterministic config error, never provider flap. The precedent already exists: a stranded `model_tiers` target is a hard boot error (`llm_pool.rs:91-98`) for exactly this reason. |
| D13 | **Strictness tracks observability, not importance.** A `model_list` entry that fails to build is still a warn+drop; a `lite_model` that fails is fatal. | A dropped pickable model is visible the moment a user opens the picker. A dropped lite is invisible — the only symptom is the bill quietly staying at main-model rates. Unobservable failures are the ones that must fail loudly at startup. Write this down or someone will "fix" the asymmetry. |
| D14 | `run_judge` retries **once** on a parse failure (not on a provider error), with a corrective instruction. Every parse failure logs a `warn!` carrying the first 200 chars of the raw reply. | `run_judge` is one call, one `extract_json_object`, one `serde_json::from_str`, zero retries; any miss is fail-closed into an approval prompt. "Respond with ONE JSON object and nothing else" is precisely what a small model fumbles, so the prompt-approval rate is the metric that decides whether lite was a good idea — and today `extract_json_object` returning `None` is completely silent. |
| D15 | WebFetch's summariser input cap becomes a function of `lite.model_info().context_window` instead of the fixed 96 KiB; `first_user_question` truncates to ~2000 chars. | `MAX_SUMMARY_INPUT_BYTES = 96 * 1024` is justified in its own comment as "well inside every modern context window" — an assumption lite breaks. `first_user_question` has no truncation at all today: a pasted 200 KB opening message goes into the title prompt verbatim (pre-existing, harmless on a 200K window, fatal on a small one). |
| D16 | Per-model overrides ship as **PR 1**, ahead of lite, in the `model_list` shape. | D15's clamp reads `context_window`; without PR 1 a lite client inherits the entry-level override written for the *main* model, and the clamp reads a number that describes a different model. Also keeps the lite diff reviewable — see the repo rule on focused feature PRs. |
| D17 | `LlmModelSpec` carries `context_window` / `pricing` / `supports_vision` only. `reasoning_effort` stays at **entry** level and is not per-model. | Owner's call. The three are *facts about a model*; effort is a knob. Consequence recorded in §6. |

---

## 2. PR 1 — `model_list`

### 2.1 Shape

```rust
pub struct LlmModelSpec {
    pub model: String,
    pub context_window: Option<usize>,
    pub pricing: Option<LlmPricingOverride>,
    pub supports_vision: Option<bool>,
}
```

`LlmEntry.model_candidates: Vec<String>` becomes `model_list: Vec<LlmModelSpec>`. Items are
always objects — no bare-string form, no serde alias for the old field name:

```json
"model_list": [
  { "model": "claude-opus-4" },
  { "model": "claude-haiku-4",
    "context_window": 200000,
    "pricing": { "input_per_1m_tokens": 1000000, "output_per_1m_tokens": 5000000 } }
]
```

A `String | object` union would buy back-compat for `model_candidates: ["a","b"]` plus terseness
for the (common) no-override case, at the cost of ~50 lines of hand-written `Serialize` /
`Deserialize` and a shadow struct. Back-compat is not wanted here, and terseness alone does not
pay for that, so `LlmModelSpec` is a plain derive.

**Normalisation, one rule:** if `entry.model` does not appear in `model_list`, prepend
`{ model: entry.model }`. Listing only the extra models is therefore equivalent to listing the
default first; both converge on `[default, …rest]`, which is what `reload.rs` already builds
`entry_models` from — and that order **is** the chat model picker's order, which is why this is a
list and not a map.

### 2.2 Entry-level fact fields are deleted

`LlmEntry.context_window`, `LlmEntry.pricing`, `LlmEntry.supports_vision` are removed outright.
Keeping them as a fallback would leave two places to write the same thing, which is the exact
defect `model_list` exists to close. No tombstone field and no `validate()` rejection: per the
repo rule against legacy-data migrations, the field and its consumers go and the orphaned key in
an old config stays inert — serde ignores it, and the model falls back to the factory's per-model
resolution.

Effective-value precedence becomes: `model_list[i]` field → factory default
(`registry.rs:99 pricing_for_model` / the OpenRouter snapshot, keyed by model slug) → per-provider
constant. Note the bug this closes is narrow by construction: an operator who set *no* override was
already getting per-model-correct values from the snapshot.

`boot.rs:174-185` stops copying `entry.supports_vision` / `entry.context_window` / `entry.pricing`
into `LlmProviderConfig` and copies the matching `LlmModelSpec`'s instead.
`entry.reasoning_effort` keeps being copied as-is.

### 2.3 Two pre-existing holes fixed here

- `reload.rs:194 refresh_pairs` maps only `e.model`, so **non-default models never get the live
  OpenRouter pricing refresh** — they run on the boot snapshot forever. Iterate `model_list`.
- (`pricing_overlay`'s gap is lite-specific and belongs to PR 2 — see §3.6.)

### 2.4 Blast radius

`crates/config/src/llm.rs`, `crates/config/src/validate.rs`, `crates/config/src/reload.rs`,
`crates/baybo/src/boot.rs:151`, `crates/baybo/src/reload.rs:49,148,194`,
`crates/gateway/src/api/dto.rs` (`LlmModelEntry` loses `supports_vision_override` /
`context_window_override` / `pricing_override`; `UpdateLlmModelRequest` loses the same three),
`crates/gateway/src/api/admin/llm.rs:125-146` (PUT) and `:497` (GET),
`docs/openapi.json`, `app/web/src/api/schema.d.ts` + the `scripts/check-ts-bindings.sh` gate,
`crates/setup/src/flow/llm.rs` (two `LlmEntry` literals), `baybo.example.json`,
`docs/modules/config.md`.

The admin **write** path gains nothing: PUT keeps editing entry-level fields only, and
`model_list` stays file-edited like `model_candidates` is today. GET surfaces `model_list`
read-only.

---

## 3. PR 2 — lite

### 3.1 Call-site inventory

Every `BillableLlm` consumer in the workspace, and where it lands:

| Call site | Location | Input | Model |
|---|---|---|---|
| Main chat | `agent_loop.rs:1618` | transcript | main |
| Tool side-LLM binding | `agent_loop.rs:1253` → `tool_executor.rs:708` | (per tool) | **lite** |
| `judge_pre_exec` | `bash/mod.rs:1821` → `judge.rs:98` | command line | **lite** |
| `judge_post_fail` | `bash/mod.rs:1878` → `judge.rs:135` | command + 2 KiB tails | **lite** |
| WebFetch summary | `web_fetch.rs:532` → `run_summary` | fetched page | **lite** |
| Title generation | `agent_loop.rs:2913` → `title.rs` | one user message | **lite** |
| Progress observer | `agent_loop.rs:2705` | **transcript prefix** | main (D3) |
| Context compression | `agent_loop.rs:2573` | **transcript** | main (D3) |
| Skill assessor | `runtime.rs:425` | skill files | main (D3) |

`crates/memory` makes no LLM calls — the "memory extraction" named in `lite_model`'s doc comment
does not exist yet.

### 3.2 Pool

```rust
pub(crate) fn resolve_lite(
    &self,
    name: Option<&LlmEntryName>,
    model: Option<&str>,
) -> (Arc<BillableLlm>, LlmEntryName)
```

Mirrors `resolve` (`llm_pool.rs:138`). The `model` parameter is load-bearing for D6's terminal
fallback: a session pinned to a `model_list` candidate with no lite configured anywhere must fall
back to **that** client, not the entry's default.

`LlmClientPool` gains `lite: HashMap<LlmEntryName, Arc<BillableLlm>>`. `entry_models` is
**unchanged** (D11).

`build_pool_clients`'s `Job { is_default: bool }` becomes a three-valued role
(`Default | Listed | Lite`). Dedup: `lite_model == entry.model` reuses the default client;
`lite_model` already in `model_list` reuses that override client. The existing `seen: HashSet`
extends by one case.

### 3.3 `ModelTier::Fast` → `Lite`

`crates/model/src/model_tier.rs` (variant, `as_str`, `parse`, `all`, tests) plus:
`crates/subagent/src/tool.rs:485` (the `spawn_subagent` JSON schema enum the parent model reads →
`["lite","balanced","deep"]`), `tool.rs:226` and `loader.rs:83` error text, `builtin.rs`
profile defaults and the `(default tier: fast)` description renderer, `docs/external-agents.md:74`,
`docs/modules/config.md`.

The coupling is intentional and must be documented: `model_tiers[Lite]` is simultaneously the
lite fallback **and** `spawn_subagent(model_tier: "lite")`'s target. An operator who wants them
separate configures a per-entry `lite_model`, which outranks the tier (D5).

### 3.4 Agent layer

`AgentLoop` gains `lite_client: Arc<BillableLlm>`, resolved in `new()` next to `llm_client` and
re-resolved inside `refresh_active_llm()` (`agent_loop.rs:700`) under the same `Arc::ptr_eq`
short-circuit. Consumers: the title runner (`:2913`) and the tool binding (`:1253`).

Subagents need no extra work — their `initial_llm` is already the tier-resolved entry, so
`resolve_lite` returns that entry's lite.

### 3.5 Tools layer

`ToolContext.llm` → `lite_llm` (`crates/tools/src/lib.rs:274`), doc rewritten; the bind site at
`tool_executor.rs:708` sources from `lite_client`. Two production readers
(`bash/mod.rs`, `web_fetch.rs`) plus ~15 test assignments.

### 3.6 Judge robustness (D14)

`run_judge` (`judge.rs:177`): on `extract_json_object` → `None` **or** a `serde_json` error, log
`warn!` with the reply's first 200 chars and retry once with an appended corrective turn. A
provider error is **not** retried — that path already has the LLM layer's own retry, and doubling
it would double the latency in front of an approval prompt.

### 3.7 Input clamps (D15)

- `web_fetch.rs:90`: `MAX_SUMMARY_INPUT_BYTES` becomes a ceiling, with the effective cap
  `min(MAX_SUMMARY_INPUT_BYTES, context_window-derived budget)` leaving room for the system prompt
  and the reply. Update the constant's comment — its "well inside every modern context window"
  justification is what lite invalidates.
- `agent_loop.rs:2952 first_user_question`: truncate to ~2000 chars.

### 3.8 Cost / reload

- `reload.rs:181 pricing_overlay` chains `clients` + `overrides`; the new `lite` map must be
  chained too, or lite models never seed `CostManager` at boot.
- `ReloadOutcome.dropped` must distinguish a dropped lite so a typo'd `lite_model` cannot report
  "reload succeeded" while the judge silently falls back to the main model. (With D12 a *build*
  failure is fatal; this covers the reporting path for the entry-dropped case in D12's second half.)
- `lite_model` must name a `model_list` item — checked in `validate()`, so a stranded reference is
  rejected by `dry_run` **without building anything**, ahead of D12's build-time check.

### 3.9 Stale comment to fix

`title.rs:52` reads "Title generation runs on the default model, not the session pin". It does not
— `agent_loop.rs:2913` passes `self.llm_client`, the session-pinned client. Only `reasoning_effort`
is not per-session. After this PR it runs on the session entry's **lite** client; rewrite the
comment to say that.

---

## 4. Tests

**PR 1**
- Per-model overrides survive deserialization, and a spec with none round-trips without gaining
  null-valued keys (every admin mutation rewrites the whole file).
- Normalisation prepends `entry.model` when absent and does **not** duplicate it when present;
  omitting the default is equivalent to listing it first.
- A config still setting entry-level `context_window` / `pricing` / `supports_vision` fails
  `validate()` with the field named.
- Two models in one entry with different `pricing` build two clients with different
  `model_info.pricing`.
- `refresh_pairs` covers every model in `model_list`, not just the default.

**PR 2**
- `resolve_lite` cascade: per-entry lite → tier → session client, including the case where the
  session pinned a non-default model and nothing is configured (must return **that** client).
- No double hop when the tier target has its own `lite_model`.
- `entry_models` is byte-identical with and without `lite_model` configured (D11).
- A surviving entry with an unbuildable `lite_model` fails boot **and** `dry_run`; a dropped entry
  with an unbuildable lite does not (D12).
- `lite_model` naming a model absent from `model_list` fails `validate()`.
- `ModelTier`: `"fast"` still deserializes and still parses; `as_str()` is `"lite"`;
  `spawn_subagent`'s schema advertises `"lite"`.
- `run_judge` retries exactly once on a malformed verdict and succeeds on the retry; a provider
  error is not retried; both paths emit the `warn!`.
- WebFetch clamps its summariser input against a small `context_window`.
- `first_user_question` truncates.
- Dedup: `lite_model == entry.model` and `lite_model ∈ model_list` each build one client, not two.

---

## 5. Docs to update

`docs/modules/config.md` (`model_list`, precedence, the tier rename, D12/D13's strictness rule),
`docs/modules/llm.md` if present, `docs/config-hot-reload.md` (the `agent.model_tiers` mentions),
`docs/external-agents.md:74`, `app/ios/docs/model-picker.md` (it already names `lite_model`),
`baybo.example.json`.

---

## 6. Known limitations, accepted

- **`judge_post_fail` runs on lite and its `safe` verdict re-runs the command on the host outside
  the sandbox with no approval** (D10). The recommendation was to keep this one judge on the main
  model, or to require the main model to confirm the privilege-granting verdict: it is the most
  privileged automatic action in the codebase, its input includes attacker-reachable
  stdout/stderr, its defence is entirely prompt-based (`POST_FAIL_SYSTEM`), and its call volume
  makes the saving negligible. Fail-closed covers an *unavailable* judge, not a *fooled* one.
  Recorded here because the trade-off should be re-openable, not because the decision is unsettled.
- **The lite client inherits the entry's `reasoning_effort` with no per-model escape hatch**
  (D17). `reasoning_effort` is baked in at construction (`completion_model.rs:58`) and the lite
  call sites pass `None` per request, which `completion_model.rs:161` resolves to the baked-in
  value. An `openai-subscription` entry configured `"high"` therefore runs its judges at high
  effort; the only remedy is changing the entry default for every session. Affects that provider
  only — nothing else reads the field.
- **`model_tiers[Lite]` is shared** between the lite fallback and subagent tier selection (§3.3).

---

## 7. Amendments found while implementing

Three places where the code does not match the plan above. Each is a decision the plan got wrong,
not a shortcut.

**A1 — there is no separate lite client build.** §3.2 planned a third `Role::Lite` build job with
dedup rules for "lite equals the default" and "lite is already in `model_list`". Both are moot:
`validate()` requires `lite_model` to name one of the entry's own models (PR 1), so by the time
`build_pool_clients` runs, that client has *already* been built as either the default or a listed
model. Assembling `BuiltPoolClients::lite` is a lookup into `clients` / `overrides`, never a
build. Two knock-on simplifications the plan asked for and no longer needs:

- `pricing_overlay` needs no new chain — every lite client is already in `clients` or `overrides`.
- `refresh_pairs` needs no new entries — `models()` already covers the lite model.

D12's hard failure survives, but it is now a guard rather than a live path: it can only fire when
one model of an entry fails to build while its default succeeds, which requires a factory that
rejects a specific model id. The *real* protection against a stranded `lite_model` is `validate()`.

**A2 — `LlmClientPool::with_candidates` became `from_config(LlmPoolConfig)`.** Adding `lite`
would have made it six positional parameters, three of them same-typed maps — invisible ordering
at the call site, which is exactly what the repo rule about `XxxConfig` structs exists for.

**A3 — `refresh_active_llm` resolves the lite client unconditionally**, before the
`Arc::ptr_eq` short-circuit on the main client. The plan filed lite under "same
pointer check"; that is wrong. The lite cascade can change while the main client does not — an
entry gaining a `lite_model`, or `model_tiers[Lite]` being re-pointed — and the short-circuit
would skip straight past it. The extra work on the unchanged path is one map lookup.
