# `ChatMessage` authorship encoding (`from_user: bool` → safer design)

> Status: **Done — options (1) + (2) landed 2026-05-23.** A **generic refactor** of
> `aura_model::ChatMessage`. Surfaced 2026-05-23 by an adversarial review of the
> subagent-notification work (shipped; see the "Background subagent results / scheduling
> invariants" notes in [`docs/modules/agent.md`](../modules/agent.md)).
>
> **What landed:** `from_user` is sealed (non-`pub`) behind intent constructors —
> `ChatMessage::user` (the sole producer of `from_user = true`), `::agent_context`,
> `::assistant`, `::system`, `::tool` / `::tool_result` — read via `ChatMessage::from_user()`.
> Provenance is now declared by the caller, and the wrong combo (`Assistant` + `from_user`) is
> unconstructable. `AgentLoop::run` derives genuineness by **whitelist** —
> `from_user = matches!(job_input, JobInput::UserChat { .. })` — so cron fires, spawned /
> subagent task prompts, and the subagent notification are all `agent_context`. The external
> subagent task (`run_external_agent`) flipped from `from_user: true` → `agent_context`.
>
> **Decision (cron / spawned visibility):** those reclassified prompts no longer satisfy the
> chat-surface `from_user` filters, so they stop rendering as user bubbles in the transcript,
> WS catch-up, and sidebar preview — intentional, since they aren't user-authored. The cron
> **inbox** still surfaces the prompt: `build_cron_message` now locates the cron row by its
> `[cron:<id>]` framing (`cron_prompt::is_framed_cron_prompt`) instead of `from_user`, because
> the framed prompt is an `agent_context` `Role::User` row indistinguishable by provenance from
> a skill reminder. The persistence boundary rehydrates through one seam
> (`storage::libsql::session::rehydrate_message`, the only place the stored `from_user` flag is
> honored). Option (3) deferred.

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

Recommendation: **(1) + (2)** — **implemented**. (3) deferred unless the type is reworked for
other reasons; it stays the right end-state if visible-but-synthetic prompts (cron / spawned)
ever need to render as a distinct non-user bubble rather than be hidden.

## Related

- `crates/model/src/message.rs` — `ChatMessage` / `Role`; sealed `from_user` field, the intent
  constructors (`user` / `agent_context` / `assistant` / `system` / `tool` / `tool_result`), and
  the `from_user()` getter
- `crates/agent/src/runtime/agent_loop.rs` — `run` whitelists `JobInput::UserChat` for
  `from_user`; `run_inner` builds the user vs agent-context turn; `append_user_message`
- `crates/agent/src/actor/cron_prompt.rs` — `is_framed_cron_prompt` / `frame_cron_prompt` share
  the `[cron:` tag prefix the inbox now keys off instead of `from_user`
- `crates/gateway/src/api/admin/chat.rs`, `crates/gateway/src/channel/route.rs` — the
  `Role::User && from_user()` visibility filter; `build_cron_message` locates the cron prompt by
  framing
- `crates/storage/src/libsql/session.rs` — `rehydrate_message`, the one seam that maps a stored
  `(role, from_user)` row back to the right constructor
