# LLM entry editor

*Settings → Models: the gateway's configured `llm` entries, and the editor for
one of them — governing `App/Screens/LlmEntriesScreen.swift`,
`App/Screens/LlmEntryScreen.swift`, `ModelCatalog`'s write door, and the
`llm_update_model` / `llm_test_model` / `llm_set_default` FFI calls.*

This is the **global config** half of the model story.
[model-picker.md](model-picker.md) is the other half: the chat header pins ONE
session. Everything here changes what the gateway is configured to do, for every
device, and — for a session with no pin — from its next turn.

## Scope, and what is deliberately absent

There is **no create and no delete for an ENTRY**: the gateway exposes no POST
or DELETE for one. Adding an entry is `baybo llm add` or a config-file edit. The
editor is for entries that already exist, and the screen says nothing that
implies otherwise.

The models an entry SERVES are a different question, and that one is managed
here — see [The model list](#the-model-list).

Two more absences, each load-bearing:

- **Provider is read-only.** The valid set is `LlmProviderRegistry`'s 19
  factories, which is enumerable in Rust and on **no HTTP route** — the phone
  cannot obtain it. And a bad provider is not a 400: `prepare` warns and DROPS
  the entry from the pool, `dry_run` passes, the file is written, and
  `GET /v1/llm/models` keeps listing a row that no longer exists. A free-text
  field here would be a silent 200-OK deletion.
- **No "clear key" affordance.** `api_key: ""` does **not** clear the vault —
  the handler logs the request and returns, leaving the prior secret in place —
  and no HTTP route deletes a vault key at all. The DTO's doc comment promises
  the opposite and has already generated into the OpenAPI schema `app/web`
  consumes; build from the handler, not the doc. The FFI refuses an empty key
  before the wire rather than reporting success for a write that did nothing.

`lite_model` and pricing are read-only over HTTP and are not rendered — a row
you cannot act on is noise. `lite_model` earns its keep only as the Model
picker's warning.

Per-model overrides for a NON-default model are still config-file only:
`PUT /llm/models/{name}` addresses the default model's spec and nothing else.
Adding a model here makes it servable and pinnable; tuning its context window or
vision flag means making it the default first, or editing the file.

## One row, one JSON key, one PUT

**There is no Save button on the fields level, and that is the design.**

`update_model` assigns `entry.model` BEFORE it resolves `default_spec_mut()`,
which is where `context_window` / `supports_vision` / `pricing` land. By then the
spec resolves against the **new** model. So a body carrying `model` beside any of
those transplants the departing model's overrides onto its successor — and
answers `200 OK`, `requires_restart: false`. Nothing rejects it: there is no
membership rule in `validate()`, and client construction is local and offline so
`dry_run` builds happily.

A client that sends exactly one key **cannot express that request**. That is the
whole reason the editor commits per row rather than per form, and it is why the
obvious `ProjectSettingsSheet` shape ("the PUT replaces the whole record, so
every field must be sent") is exactly wrong here.

To be precise about the severity: no in-tree client triggers this today.
`app/web` dirty-tracks every field, so a `context_window` it sends was just
typed and is meant for the new model; the CLI never goes through HTTP at all.
It is a hazard in the API's shape, waiting for the next client written the
obvious way — read the entry, render a form, PUT the form back. One observable
consequence IS already live, though: `default_spec_mut()` permanently
materialises the departing default into `model_list`, `UpdateLlmModelRequest`
has no field for it, and `model_list` is what feeds `entry.models()` — so the
picker's candidate list grows with every default-model switch that followed an
override, on both clients, with no way to shrink it.

It also dissolves a UniFFI limit for free. The endpoint is three-state per field
— key absent = keep, `null` = clear, a value = set — which wants
`Option<Option<T>>`, and UniFFI cannot express that. But a single-field edit **is
a sum type**: `LlmEntryEdit` has one variant per row, `None` means `null`, and
"keep" is simply a variant nobody sent. `entry_edit_body` is the one place that
mapping lives, and **no `skip_serializing_if` may ever appear in it** — that
would silently turn every clear into a keep.

The same rule covers a second hazard: the vault write lands **before** the
pre-flight that can reject it, so a request rotating a key alongside an
unbuildable change returns 400 with the config untouched and the secret already
replaced. Combined with the un-clearable key above, that is unrecoverable from
the phone. A key that rides alone cannot be in such a request.

## The model list

`model_list` is the set of models an entry serves — what
`LlmEntry::models()` returns, what the pickers offer, and what a session pin may
name. It is also the per-model override table, which is why membership and
overrides travel together.

Until `PUT /llm/models/{name}/model-list` existed it could only GROW: the only
writers were the config file, the setup wizard's `lite_seed` (which fires for
one provider), and `default_spec_mut()` materialising a departing default. No
HTTP route could shrink it, so a model the operator had moved off stayed in
every picker forever.

**The endpoint replaces the whole SET, and that is deliberate on two counts.**
Model ids routinely carry a slash (`meta-llama/Llama-3-70B`), which rules out a
path segment for add/remove; and a whole-set PUT is idempotent, so a relay leg
that replays it converges instead of double-adding. Replacing never destroys an
override — the handler carries each surviving id's spec across and only mints a
bare one for an id that was not there — so a client may send plain ids without
echoing back config it does not understand.

Three refusals, each protecting something a silent success would break:

- **the default model must stay in the list.** `models()` prepends it when
  absent, so dropping it would not drop it — the write would report a set the
  entry does not have;
- **no duplicate ids.** `spec_for` returns the first match, so a second copy's
  overrides would be permanently inert and invisible in every read-back;
- **`lite_model` may not be stranded.** The config validator owns that rule and
  its message already says how to fix it; the endpoint just has to run it before
  writing.

**Adding is a PICK from `GET /llm/models/{name}/catalog`, never free text.**
That route asks the provider what it currently offers, marking what the entry
already serves. It exists because nothing gateway-side checks a model id against
the vendor: a typo builds a client, gets listed in `entry_model_ids`, passes the
session-pin validator, and only fails at the first real completion. The read is
live and uncached — a stale catalog would offer models the account may no longer
have — and it carries the same 45s client budget as the probe, for the same
reason.

## Not optimistic, and the write re-reads

Every write ends in a forced `ModelCatalog.reload()`, awaited under the caller's
spinner. This is not caution — the row's `effective*` columns are layered
gateway-side over the OpenRouter snapshot and the provider's factory defaults,
tables the app does not have. Clearing an override, or changing `model`, MOVES
values nothing here can compute, so a locally-guessed row would disagree with
the gateway.

`reload()` exists because `refreshIfNeeded()` **latches permanently**:
`fetchedThisRun` is set on success and `fetchTask` is cleared only in the failure
branch, so after the first success the only thing that makes it fetch again is
`unload()` — which empties `models` and would blank the chat header's pill
mid-session.

Two rules on top of it:

- **`reload(expecting:)` takes the epoch the CALLER started at**, not the one it
  finds. A write that began before a logout and lands after it would otherwise
  re-read on the new binding's epoch, pass its own guard, and repopulate a fresh
  catalog with the departed gateway's entries.
- **A failed read-back keeps the rows it has** and rethrows. The write already
  landed; emptying the catalog because the read-back blipped is a worse lie than
  values that are one edit stale, and it would blank the pill too.
- **A failed write never reaches the mirror.** Writing optimistically would
  resurrect a refused edit on the next cold launch with nothing to correct it.

## Inherited vs pinned

The screen's whole state language is one colour step: a **soft** value is
inherited from the provider's capability chain, an **ink** value is an override
pinned on this entry. It is not self-explanatory, so each editor says which in
words and `accessibilityValue` spells it out ("400000, set on this entry" /
"200000, inherited from the provider").

The API-key row is the exception and takes a plain value: that phrasing is about
an *override*, and a write-only credential has none.

**The seeding trap this exists to disarm:** the row carries
`effective_context_window` beside `context_window_override`, so a field seeded
with the effective number and saved untouched would MANUFACTURE an override out
of an inherited value, pinning the model to a number that used to track the
snapshot. Save stays disabled while the draft equals the seed.

Vision is **three rows, never a `Toggle`**, for the same reason: a two-state
control cannot express *clear*, so it would silently convert an inherited `true`
into a pinned `true` that stops tracking the provider forever.

## `api_key_configured` is not an error

It means a key **resolves** — across the env var, the vault, and the provider
default — not that one is stored. And `false` is a perfectly healthy state:
`ollama` takes an optional key and `llamafile` none at all. It renders in
`inkSoft`, never `Theme.err`; colouring it would train the eye to ignore the one
hue this design reserves for state.

**An env var outranks the vault.** Resolution order is the entry's explicit
`api_key_env`, then the per-entry vault key, then the provider default. So a key
typed while `api_key_env` is set is accepted, reported saved, flips
`api_key_configured` to true — and is inert. The key screen warns on exactly that
pair before the tap, and offers the escape hatch (`api_key_env` IS clearable).

**On an `http://` direct binding the key field is replaced by the reason it is
absent.** `normalize_base` preserves an explicit `http://` on purpose, so the
body would cross the network in clear text beside the admin bearer. The
predicate is `active_binding_is_cleartext()` in the FFI, not a `hasPrefix` in
Swift: it is a fact about the transport, and the transport is the one place that
gets to answer it.

## Test connection

`POST /v1/llm/models/{name}/test` is a **real, billed completion** and the only
vendor-touching pre-flight the system has. Two constraints:

- **`post_json_once`, never `post_json`.** The relay replays a pooled leg that
  went silent, and this probe routinely outlives that budget — the gateway allows
  60s to connect and a 600s idle read with no total cap, against a
  15s-to-first-byte pooled leg. A replay is a second billed completion, and an
  invisible one: the probe runs with cost hooks passed through, so it never lands
  on a `cost_records` row.
- **`LLM_PROBE_TIMEOUT` (45s) is the client's own budget.** Neither leg bounds
  it: the relay's 30s would report a misleading transport error, and the direct
  leg's JSON client applies no whole-request timeout at all, so an unanswered
  probe would hang until the provider gave up.

**The button is HIDDEN, not captioned, once a write comes back staged.**
`test_model` reads the config from **disk** while the running pool still holds
the old client, so in exactly that state a green probe certifies settings nobody
is serving. Reading `requires_restart` at all is why `put_json` had to exist —
`put_empty` discards the body, and persisted-but-not-live would be
indistinguishable from live.

`requires_restart` itself means *on disk, not in the pool*: the reloader stored
the new baseline and returned before rebuilding, which happens when some other
non-hot field was already pending on disk.

## The outcome strip sits above the level content

A pick commits from a SUB-level. A strip that rendered only on the fields level
would leave the picker showing nothing at all after a failure — which reads as
"the tap did nothing" rather than "the gateway refused it". Both legs collapse a
non-2xx into a bare status code, so the copy is generic on purpose; the probe's
`error` is the one place a gateway-side failure arrives as prose.

## Tests

- `crates/gateway/tests/llm_endpoint.rs` — the list endpoint's add/remove, that
  a surviving id keeps its overrides, the three refusals, and that a replay
  converges rather than doubling.
- `ffi/src/gateway_api.rs` — one key per body and explicit nulls, the empty-key
  refusal, `POST_ONCE` for the probe, `requires_restart` surviving the decode,
  path escaping, the widened narrow (both the override and effective columns,
  and that a bare row keeps them apart), the list riding the body with its
  slashes intact, and `configured` defaulting to false.
- `Tests/LlmEntryEditorTests.swift` — the write re-reads rather than guessing, a
  cleared override takes the server's effective value, a failed write never
  reaches the mirror, a failed read-back keeps its rows, the logout straddle
  (which needs `stallLlmWrite` to actually BE a straddle), and an older mirror
  still decoding.
- `UITests/LlmEntryUITests.swift` — `-baybo-open-home -baybo-home-tab settings
  -baybo-demo-models`. Every READ on this path is real; a write has no gateway
  behind it, so the smoke drives it for the failure surfacing instead.
- `Tests/Support/LlmFixtures.swift` — the one place a test builds an
  `LlmModelInfo`. The record went from six fields to fourteen here and touched
  every hand-rolled fixture in the suite; the next field costs one default.
