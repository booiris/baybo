# `ChatMessage` authorship encoding (`from_user: bool` → safer design)

> Status: **TODO, not started.** A **generic refactor** of `aura_model::ChatMessage` — it
> should land on `master` on its own, not bundled into a feature branch. Surfaced 2026-05-23
> by an adversarial review of the subagent-notification work (see
> [`subagent-notification-and-message-priority.md`](subagent-notification-and-message-priority.md)).

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

Recommendation: **(1) + (2)**. Defer (3) unless the type is being reworked for other reasons.

## Related

- `crates/model/src/message.rs` — `ChatMessage` / `Role`; the `from_user` field + its doc comment
- `crates/agent/src/runtime/agent_loop.rs` — `run` derives `from_user` from the job kind;
  `run_inner` builds the user turn; `append_context_message` / `append_user_message`
- `crates/gateway/src/api/admin/chat.rs`, `crates/gateway/src/channel/route.rs` — the
  `Role::User && from_user` visibility filter the encoding feeds
- `crates/storage/src/libsql/session.rs` — the persisted `from_user` column (matters for option 3)
