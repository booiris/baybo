# Agent model allow-list + reasoning effort

Design spec, 2026-07-10. Adds two behavior fields to `AgentProfile`
([`../modules/agent-profiles.md`](../modules/agent-profiles.md)): an
**allowed-models set** that restricts which LLM entries a bound session may
switch to, and a **reasoning effort** setting delivered per-request to the
LLM layer (consumed by the `openai-subscription` provider in v1; the plumbing
is generic so other providers opt in later).

Depends on multi-agent chat Phase 1 (session→agent binding,
[`multi-agent-chat.md`](multi-agent-chat.md)) — branch stacks on it.

## Requirements

- **Allowed models** (`allowed_models`): a set of `baybo.json` LLM entry
  names. Empty/absent = unrestricted (today's behavior). For a session bound
  to an agent with a non-empty set, the model switcher offers only set
  members and `PUT /v1/chat/sessions/{id}/model` rejects names outside it.
  Clearing the pin (follow `default-llm`) is always allowed — the set
  constrains explicit choices, not the system default.
- **Reasoning effort** (`reasoning_effort`): one of
  `minimal | low | medium | high | xhigh`; absent = follow the LLM entry's
  own configured value (status quo). Carried on every main-loop LLM request
  of bound sessions; in v1 only the `openai-subscription` (Codex Responses)
  provider consumes it, overriding its entry-baked value. Other providers
  ignore the field (documented consumption matrix).
- Both fields are baybo-framework-only (stored regardless, greyed for
  external frameworks — same treatment as the `llm` pin) and locked on the
  builtin profile like every other content field.

## Data model & storage

Two new columns on `agent_profiles` (guarded ALTER + CREATE TABLE, house
encodings):

```sql
allowed_models   TEXT,  -- JSON array of LLM entry names; NULL or '[]' = unrestricted
reasoning_effort TEXT   -- ReasoningEffort::as_str(); NULL = follow the entry's value
```

- `AgentProfileRow` / `AgentProfileUpdate` gain
  `allowed_models: Vec<LlmEntryName>` (empty = unrestricted; storage
  normalizes NULL ↔ empty) and `reasoning_effort: Option<ReasoningEffort>`.
- `ReasoningEffort` is a new `baybo-model` unit enum
  (`Minimal | Low | Medium | High | XHigh`) with the house
  `as_str()` / `parse()` / `const ALL` mirror. The Codex provider's
  per-model-family clamp tables keep working on `&str` — the enum's
  `as_str()` feeds them; no provider-side table rewrite.
- Unknown `reasoning_effort` string on read is an error (agent-profiles read
  rule); a malformed `allowed_models` JSON blob likewise.

## API & validation

`AgentProfileDto` / `CreateAgentProfileRequest` / `UpdateAgentProfileRequest`
gain `allowed_models: Vec<String>` (default empty) and
`reasoning_effort: Option<String>`. Full-replace `PUT` semantics unchanged.
Write-time validation (shared helper beside `validate_llm_pin`):

| Rule | Failure |
|---|---|
| every set member is a live pool entry (`validate_llm_pin` per name) | 400 |
| set is de-duplicated, order preserved | (normalize, not an error) |
| `llm` pin set AND set non-empty → pin ∈ set | 400 |
| `reasoning_effort` parses via `ReasoningEffort::parse` | 400 listing legal values |

**Model-switch enforcement**: `PUT /v1/chat/sessions/{session_id}/model`,
after the existing `validate_llm_pin`, loads the session's bound profile
(when `agent_id` is set); if its `allowed_models` is non-empty and the
requested name ∉ set → 400 ("model {name} is not in agent {agent}'s allowed
set"). `llm: null` (clear the pin) bypasses the set check.

**Read-time tolerance** (house style): a set member later removed from
`baybo.json` renders "(unavailable)" in the editor and simply never matches
at switch time; a historical pin outside a later-shrunk set is tolerated —
actor spawn warns and the pool's normal resolve applies, nothing rewrites
the session.

## Runtime: effort delivery

1. `ChatRequest` gains `reasoning_effort: Option<String>`
   (`#[serde(default, skip_serializing_if = "Option::is_none")]`, beside
   `temperature`).
2. The router's spawn-time profile fetch (the session-pin-vs-profile-pin
   resolver, renamed `resolve_spawn_llm` now that it also carries effort)
   expands to also carry the profile's `reasoning_effort`, returning a
   `SpawnLlmChoice { initial_llm, reasoning_effort }` to the actor spawner —
   the profile is fetched even when the session's own pin is already set,
   because `reasoning_effort` only ever comes from the profile. It lands on
   `AgentLoopConfig.reasoning_effort: Option<ReasoningEffort>`;
   `AgentLoop::call_llm` fills the request field from it. Side-calls (title
   generation, progress observer, compression) deliberately send `None` —
   they stay cheap. Liveness matches the `llm` pin's spawn-time half: a
   profile edit lands on the next actor spawn/hydration; unlike the `llm`
   pin, there is no live re-pin — `AgentMessage::SetModel` only ever carries
   a model name, so an in-flight actor's effort is fixed until it is next
   (re)built.
3. rig's `CompletionRequest` has no native reasoning-effort field, so
   `LlmClient::chat` / `chat_stream` bridge `ChatRequest.reasoning_effort`
   through `additional_params` — a provider-opaque JSON bag rig already
   threads to every backend — under a namespaced key
   (`baybo_reasoning_effort`), set only when the target provider variant
   opts in via `AnyCompletionModel::accepts_request_reasoning_effort()`
   (today only `OpenAiSubscription`), so no other provider's request body
   ever carries the key. `openai-subscription` provider: keeps its
   construction-baked value as the fallback; per-request handling reads the
   key back out of `additional_params` and prefers it over the baked value
   when present, fed through the existing `resolve_effort` per-model-family
   clamp (out-of-range values clamp with a `warn!`, never fail the call).
   All other providers ignore the field entirely — they never see the key;
   [`../modules/llm.md`](../modules/llm.md) documents the consumption matrix.

## Web

- **Agents editor** (`AgentsPage.tsx`), below the `llm` pin: a model
  multi-select (checkbox row per `llmNames` entry; none checked =
  unrestricted) and an effort select ("Default (entry setting)" +
  the five values). Both grey out for external frameworks alongside the pin.
  When the set is non-empty the pin select offers only set members
  (client-side mirror of the server rule).
- **ModelPicker** (`ChatPage.tsx`): for a session bound to an agent with a
  non-empty set, filter the picker to set members; if the currently-effective
  model is outside the set, show it flagged "(not in allowed set)" rather
  than hiding it. Agent data comes from the existing `GET /v1/agents` fetch —
  no new endpoint, no new WS frames.

## Error handling

| Failure | Behavior |
|---|---|
| unknown entry name in set / pin ∉ set / bad effort at write | 400 |
| switch to a model outside the bound agent's set | 400 |
| set member removed from pool later | editor "(unavailable)"; never matches at switch |
| historical pin outside a shrunk set | tolerated; spawn `warn!` + normal resolve |
| effort value out of a model family's clamp range | provider clamps + `warn!`, call proceeds |

## Testing

- storage: column round-trips (JSON array ser/de, NULL ↔ empty Vec,
  bad-JSON / bad-effort read errors), builtin lock untouched.
- model: `ReasoningEffort` parse/as_str/ALL round-trip.
- gateway: write-validation matrix; `PUT …/model` set enforcement
  (bound + non-empty / bound + empty / unbound / clear-pin bypass / member ok
  / non-member 400).
- agent: `call_llm` fills `reasoning_effort` from config (StubLlm captures
  the request); side-calls stay `None`.
- llm: openai-subscription request-value-overrides-baked unit test (existing
  reasoning.rs test style).
- web: editor save round-trip incl. new fields; picker filtering component
  behavior. Standard openapi regen chain (no ts-rs wire change).

## Collaboration

| Module | Role |
|---|---|
| `model` | `ReasoningEffort` enum |
| `store` / `storage` | row/update fields + two columns |
| `llm` | `ChatRequest.reasoning_effort`; openai-subscription per-request override |
| `agent` | spawn-time profile read extension; `AgentLoopConfig.reasoning_effort`; `call_llm` fill |
| `gateway` | profile write validation; model-switch set enforcement; DTOs + openapi |
| `web` | editor controls; ModelPicker filtering |
