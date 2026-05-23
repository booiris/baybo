# `ChatMessage` authorship encoding (`from_user: bool` → safer design)

> Status: **Done — (1) + (2) landed 2026-05-23; the typed-provenance step (a focused form of
> (3)) landed 2026-05-24.** A **generic refactor** of `aura_model::ChatMessage`. Surfaced
> 2026-05-23 by an adversarial review of the subagent-notification work (shipped; see the
> "Background subagent results / scheduling invariants" notes in
> [`docs/modules/agent.md`](../modules/agent.md)).
>
> **What landed (1 + 2):** the provenance field is sealed (non-`pub`) behind intent constructors —
> `ChatMessage::user` (the sole user-authored producer), `::agent_context`, `::assistant`,
> `::system`, `::tool` / `::tool_result`. Provenance is declared by the caller, and the wrong combo
> (`Assistant` + user-authored) is unconstructable. `AgentLoop::run` derives provenance from the
> `JobInput` kind, so cron fires, spawned / subagent task prompts, and the subagent notification
> are never marked user-authored. The external subagent task (`run_external_agent`) flipped to
> agent context.
>
> **What landed (typed provenance, 2026-05-24):** the `from_user: bool` field was replaced by a
> sealed `source: MessageSource { User, Cron, Agent }` enum, plus a `ChatMessage::cron_fire`
> constructor (the sole `Cron` producer) and a `source()` getter; `from_user()` stays as a
> convenience (`source == User`). `AgentLoop::run` maps `JobInput` → source (`UserChat` → `User`,
> `Cron` → `Cron`, else `Agent`). The **cron inbox now identifies the cron row by
> `MessageSource::Cron`** instead of sniffing the `[cron:<id>]` framing out of the content —
> `cron_prompt::is_framed_cron_prompt` was removed. `frame_cron_prompt` still prepends the
> `[cron:]` tag (LLM diagnostics + trace tooling) and `original_cron_prompt` still strips the
> framing for display. The persisted column became `source TEXT` (was `from_user INTEGER`), and
> rehydration maps `(role, source)` back to the constructor through one seam
> (`storage::libsql::session::rehydrate_message`). **No history backfill** — greenfield decision.
>
> **Decision (cron / spawned visibility):** synthetic prompts still don't satisfy the chat-surface
> `from_user()` filter, so they don't render as user bubbles in the transcript, WS catch-up, or
> sidebar preview — intentional, since they aren't user-authored. The cron **inbox** still surfaces
> the prompt, now via the typed `source` rather than the framing tag. With a typed `Cron` source in
> hand, a future surface *could* choose to render cron fires as a distinct (non-user) bubble; that
> is now a one-line filter change rather than a content sniff.

## Problem

`ChatMessage { role: Role, content, from_user: bool }` encodes message provenance across two
fields, and the `bool` is bug-prone:

- **Boolean blindness + partial field.** `from_user` is only meaningful when `role == User`
  (it distinguishes a genuine channel input from an agent-injected `Role::User` row — skill
  reminders, cron framing, the subagent notification). For `Assistant` / `System` / `Tool` it is
  always `false` and meaningless, so invalid combos like `role=Assistant, from_user=true` are
  representable.
- **Asymmetric, dangerous default.** Of ~85 construction sites almost all are `from_user: false`
  (synthetic / reconstructed); `true` (genuine user) is the rare value that must be written
  deliberately. A shared "append the user turn" path that hardcodes `true` silently mislabels any
  new caller that reuses it for non-genuine content.
- **Provenance is decided implicitly by *which code path* builds the message,** not declared by
  the caller. This is exactly how the subagent-notification regression happened: the synthetic
  XML was routed through `run_inner`'s genuine-user append (hardcoded `from_user: true`) and
  surfaced as a fake user-authored bubble. The old `background_notice` path got it right by using
  a *separate* `append_context_message(from_user = false)` injection path.
- **Presentation concern on a core type.** `from_user` is consumed only by the chat surfaces
  (`gateway` REST/WS) for visibility, yet it rides on the `ChatMessage` that flows through
  context, llm, storage, and trace.

The current feature-branch fix (`AgentLoop::run` derives `from_user` from
`JobInput::SubagentNotification`) plus a `from_user == false` regression test is enough to ship
safely, but it patches the symptom rather than the encoding.

## Proposed Direction

1. **Smart constructors + seal the field (recommended, best value).**
   Make `from_user` non-`pub` and add intent-named constructors: `ChatMessage::user(content)`
   (genuine), `ChatMessage::agent_context(content)` (injected `Role::User`), `::assistant(...)`,
   `::tool_result(...)`, `::system(...)`. With no raw `from_user:` literal to flip, every call
   site states intent and the wrong combo is unconstructable. Mechanical change across ~85 sites;
   **does not touch the storage column, llm conversion, or ts-bindings.**
2. **Path separation (cheap, treats this root cause).** Keep the bool, but reserve `run_inner`'s
   user-turn append for genuine user input and route *all* synthetic `Role::User` content through
   one explicit "inject agent context" method that forces `from_user = false` — i.e. restore the
   pre-merge `background_notice` shape. Pairs naturally with (1) and removes the implicit
   "provenance follows the code path" trap.
3. **Single origin enum (ideal, most expensive).** Replace `role + from_user` with
   `enum MessageOrigin { User, Assistant, AgentContext, ToolResult, System }`; derive `llm_role()`
   and `is_user_visible()` from it. Invalid states become unrepresentable. Cost: the
   `session_messages` column, llm role conversion, ts-bindings, and ~85 sites all move. Worth
   doing only with dedicated budget.

Recommendation: **(1) + (2)** — **implemented**, then a **focused form of (3)**: rather than fold
`role` into a single origin enum (which would move llm-role conversion and the persisted role
column), a separate sealed `source: MessageSource { User, Cron, Agent }` was added alongside
`role`. This keeps `role` as the LLM-facing field while giving operator surfaces a typed
provenance — enough to retire the cron `[cron:]` content-sniff. The full single-enum merge stays
deferred (it would also subsume `role`); the current split is the better cost/value point.

## Related

- `crates/model/src/message.rs` — `ChatMessage` / `Role`; the sealed `source: MessageSource`
  field, the `MessageSource { User, Cron, Agent }` enum, the intent constructors (`user` /
  `cron_fire` / `agent_context` / `assistant` / `system` / `tool` / `tool_result`), the `source()`
  getter, and the `from_user()` convenience
- `crates/agent/src/runtime/agent_loop.rs` — `run` maps `JobInput` → `MessageSource`; `run_inner`
  builds the user / cron-fire / agent-context turn; `append_user_message`
- `crates/agent/src/actor/cron_prompt.rs` — `frame_cron_prompt` / `original_cron_prompt` (the
  `[cron:]` tag is for LLM diagnostics + display stripping; row identification is by
  `MessageSource::Cron`, not the tag)
- `crates/gateway/src/api/admin/chat.rs`, `crates/gateway/src/channel/route.rs` — the
  `Role::User && from_user()` visibility filter; `cron_message_from_session` locates the cron
  prompt by `source() == MessageSource::Cron`
- `crates/storage/src/libsql/session.rs` — `rehydrate_message`, the one seam that maps a stored
  `(role, source)` row back to the right constructor; the `source TEXT` column lives in
  `crates/storage/src/libsql/mod.rs`
- `web/src/types/trace.ts` — the hand-maintained `ChatMessage` mirror now carries
  `source: MessageSource` ('user' | 'cron' | 'agent')
