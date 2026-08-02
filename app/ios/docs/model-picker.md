# Model picker

*The chat header's model capsule, the hand-rolled `ModelMenuPanel`, the global model catalog, and the per-session `(entry, model, effort)` pin — governing `ChatHeaderView`, `app/ios/App/Screens/ModelMenuPanel.swift`, `ChatStore.modelPin`, and `app/ios/App/Core/ModelCatalog.swift`, plus the root-workspace seam they talk to.*

## The header capsule

The chat header holds the model capsule on the LEFT (beside the glass back circle). Its mono(15) label is the effective entry's MODEL id, **string-ellipsized** — **never cap a `Menu`/pill label with `.frame(maxWidth:)`**, it renders greedy at the cap.

### Connectivity indicator

Connectivity shows **NOTHING** in every healthy state. Only a SUSTAINED outage surfaces, as a red `wifi.slash` in a glass circle on the trailing edge.

It is keyed off `ChatStore.legDown` — a **DEBOUNCED** signal (default 4s away from `.connected`, cleared the moment a dial lands). The debounce is load-bearing: raw `connState` oscillates `connecting ↔ offline` through the retry loop on a real outage (`offline` is only the gap between failed dials, and a dead network's dial hangs in `connecting`), so an `offline`-gated indicator almost never showed.

## ModelMenuPanel

The pill toggles `ModelMenuPanel`, a HAND-ROLLED glass panel that `ChatScreen` overlays (`modelMenuOpen`) — **not** a system `Menu`, which cannot do any of:

- trailing checkmarks (selection and label icons render LEADING on iOS 26),
- removing a submenu row's `›`,
- a live subtitle.

Width knob: `ModelMenuPanel.panelWidth` (232).

### The three levels

Three levels replace in place, sub-levels headed by a back row:

1. **Entries by name** — trailing ✓ on the effective entry, no chevron. Nothing
   else: the **per-session resync** briefly sat here below a hairline and now
   lives on the chat row's long-press menu
   ([transcript.md](transcript.md#per-session-resync-the-escape-hatch)). It never
   had anything to do with the model, and the capsule is hidden while
   `ModelCatalog` has no entries — a first run that has never reached
   `GET /v1/llm/models` could not reach it at all.
2. **That entry's models** (`model` + `model_list`, ✓ on the effective model) plus, below a hairline, the **Thinking** row — subtitled with the entry's current level and carrying the panel's one trailing `›`.
3. **The levels** — whatever the entry's `available_efforts` lists, in that order. The rungs come from the SERVER, not a local list: each provider speaks its own effort vocabulary, so offering one its dialect cannot say would be a pick that never reaches the wire. An entry whose provider baybo tells nothing shows **no Thinking row at all** (level 2 hides it). `EffortLevel` only supplies the localized labels — a rung baybo learns later still renders, as its raw value.

### Accessibility contract

Panel rows set `accessibilityLabel` = title and `accessibilityValue` = subtitle, so the by-label UI smokes keep working and VoiceOver reads "Thinking, Ultra high".

## The catalog

The catalog (`GET /v1/llm/models`, FFI `llm_list_models`, narrowed to name/provider/model/**model_list**/reasoning_effort/**available_efforts**) is **global** and cached **per app run** in `ModelCatalog.shared`, plus a **`models.json` mirror** (the `deck.json` idiom, written on fetch + effort edits, deleted on logout/rebind) for offline cold-paint.

Because of the mirror, a cold offline start still paints the pill and the panel. The pill renders only once the catalog has entries, and the pill **NEVER shows a placeholder** — always the best-known model id.

## The pin

**The pin is an `(entry, model, effort)` TRIPLE** — `ChatStore.modelPin` / `modelPinModel` / `modelPinEffort`:

- the **entry** is `SessionState.last_llm`;
- the **model** is a `model_list` id in `last_model` (`nil` ⇒ entry default);
- the **effort** (thinking level) is **PER-SESSION**, not a global entry edit.

`ChatStore.selectEffort` pins (entry, its relevant model, effort); `selectModel` keeps the current effort; both funnel through `applySelection` → one PUT. The panel's Thinking checkmark/subtitle read `store.modelPinEffort ?? entry's default` (session value wins, applies whatever entry — effort is INDEPENDENT of the llm/model pin).

Pin + level apply from the session's NEXT turn — no confirmation UI.

### Re-read on EVERY open

The pin is re-read on EVERY chat open off the session detail with `limit=1` (FFI `chat_session_model` → `SessionModelPin{llm,model}`) — **never latched once-per-store**: the store stays cached resident and no frame broadcasts a re-pin made on another client, so the open edge is the only sync point.

### Writes

Writes go over `PUT /v1/chat/sessions/{id}/model` (FFI `chat_set_session_model(llm, model)`, both explicit-nullable — `{"llm":null,"model":null}` = follow `default-llm` + default model).

They are **SERIALIZED** through `enqueueModelPut`, because overlapping PUTs have no wire order.

They are optimistic: a failed PUT reverts to the last **GATEWAY-ACKNOWLEDGED** pair (`confirmedModelPin`) — never the previous display value, which would resurrect a pick the gateway refused. A seed read never applies over a bumped epoch or an in-flight PUT.

### Picking on a draft

A pick on a DRAFT stashes in `pendingModelPin` (epoch-stamped; a newer pick supersedes it) and is applied INSIDE `ensureRemoteSession`'s coalesced task, between session creation and every awaiter's send, so the first turn already runs on the choice.

That failure path **defers its notice** (`draftPinFailed`, surfaced after the send — including the recovery path — because the dial clears `notice` as it starts).

## Server-side seam

Root workspace, **NOT** covered by `app/ios` CI:

- `LlmEntry.model_list` / `lite_model` (config). The LLM pool pre-builds a client per listed model; `resolve(name, model)` picks it.
- Effort is a rung on baybo's own ladder (`baybo_llm::effort`: `off/minimal/low/medium/high/xhigh/max`), translated per provider on the way out — `reasoning_effort` for the OpenAI dialect, `output_config.effort` for Anthropic, `generationConfig.thinkingConfig.thinkingLevel` for Gemini, and Codex's own body for `openai-subscription`. The per-request `ChatRequest.reasoning_effort` is the session pin; the entry's configured level fills in when it is absent. A rung a provider's dialect cannot express fails the ENTRY at startup; the level a call actually ran at is what lands on its `cost_records` row.
- `GET /v1/llm/models` carries `available_efforts` per entry — the rungs that provider can be told. Empty for providers baybo has no effort wiring for (`baybo_llm::providers::EFFORT_WIRES` is the table; a registered provider missing a row fails a test).
- `last_model` + `last_effort` are their own flat SQLite columns (`set_last_model` / `set_last_effort`, additive migrations — keep `last_llm`'s golden JSON).
- `AgentMessage::SetModel{llm, model, effort}` threads the triple through the spawner (`ActorSpawner`'s `initial_effort`) → `AgentLoop.initial_effort` → every turn's `ChatRequest`.
- `validate_llm_model` rejects a model outside the entry's `entry_model_ids`; the pin's effort is parsed against the ladder (`ReasoningEffort::parse`) and **canonicalised** on the way in, so `none` and `off` never persist as two spellings of one rung.
- `PUT /v1/chat/sessions/{id}/model` carries `{llm, model, reasoning_effort}`; the old global `llm_set_reasoning_effort` FFI was removed (superseded).
