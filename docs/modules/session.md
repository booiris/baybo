# session - Session Lifecycle Manager

## Overview

The `session` crate owns the session lifecycle vertical: `SessionError` and the `SessionManager` business-logic facade. The `SessionStore` / `SessionSummaryStore` traits and their per-row `StoredMessage` / `SessionSummaryRow` value types live in the `baybo-store` ports crate; domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`, `LineageKind`) live in `baybo-model`. The sqlite implementations of both store traits live in `baybo-storage`, which implements the `baybo-store` contracts; `baybo-session` calls them.

A `Session` is the top of one trace tree. There is exactly one trace per session — subagent spawn creates new sessions with `Lineage` pointers, never new trees rooted in the same session.

The conversation transcript itself is **not** carried on `Session`. It lives in `baybo_context::ContextManager` while the actor is alive; the agent loop persists each appended message and each `/compact` apply through `SessionManager::append_session_message` / `apply_session_compaction`, and the actor's run loop seeds it on cold start via `ContextManager::restore_from_store`, which calls `load_active_session_messages`.

**Design principle**: each domain crate owns its business-logic vertical (manager + error + test-support fake) while the `*Store` trait contract lives in the `baybo-store` ports crate. Downstream callers depend on the manager crate for logic and on `baybo-store` for the trait. `baybo-storage` keeps only the sqlite implementations and exposes a `Store` bundle for assembly-layer wiring. Higher-level orchestration (Router, Actor) lives in `agent`, which re-exports `SessionManager` for convenience.

## Manager surface

`SessionManager` wraps three stores — `store: Arc<dyn SessionStore>` (session rows + transcript), `summary_store: Arc<dyn SessionSummaryStore>` (per-session summary-cursor metadata), and `folder_store: Arc<dyn SessionFolderStore>` (chat-list folders) — all required at `new()`. It exposes:

- Lifecycle: `create_session` / `create_session_with_trigger` / `create_spawned_session` / `get_or_create` / `get` / `list` / `list_by_channel` / `delete` / `idle_sessions` / `touch`. `idle_sessions(threshold)` only **returns** the IDs of sessions idle past the threshold — it never deletes a row (its sole consumer is the actor reaper, which evicts in-memory actors, not rows).
- Transcript brokerage (thin pass-throughs to `SessionStore`): `append_session_message`, `append_session_message_idempotent`, `apply_session_compaction`, `load_active_session_messages`, `load_active_session_messages_up_to`, `active_index_of_ordinal`, `count_active_messages`, `latest_session_ordinal`, `load_session_messages_with_supersede`, `history`, `full_transcript`. The idempotent append atomically claims a per-session `source_event_id` for crash-replayed internal notifications; ordinary turn rows use the regular append. The agent loop keeps the in-memory `ContextManager` synchronized with both outcomes.
- Chat-list + sync surface: the per-row toggles `set_hidden` / `set_pinned` / `set_archived` / `set_folder` / `set_title` / `set_last_llm` / `set_read_cursor` / `unread_reply_count`; folder CRUD `list_folders` / `create_folder` / `rename_folder` / `reparent_folder` / `reorder_folders` / `delete_folder` (depth and cycle validation live here, not in the store); pagination and point lookups `history_tail` / `history_since` / `find_message_ordinal_by_platform_msg_id`; the grouped chat-list reads `chat_list_scan` (first-line previews + second-line tail windows + unread counts for a whole visible list in three window-function queries) / `session_titles` / `session_channels`; control events `append_control_event` / `list_control_events` / `list_control_events_in_range` (page-scoped); and summary-cursor metadata `summary_metadata` / `record_summary_success` / `record_summary_failure`.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. the TUI mints a bare UUID v4 per conversation, cron mints a fresh `cron-<uuid>` per fire.

**Invariant**: when `get_or_create` needs to insert a new session (the id was missing), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

### Trigger and lineage are orthogonal

`Session.trigger: TriggerSource` records the **business source** that started this session (`User`, `Cron { cron_job_id }`). `Session.lineage: Option<Lineage>` records the **parent relationship** when the session was spawned from another (`Subagent`).

A spawned session's `trigger` is **inherited from the root** session (so a subagent spawned by a cron-triggered session has `trigger = Cron { ... }`), making "is this work cron-driven?" an O(1) field read. `Lineage` separately records the direct parent.

### Root-session ancestry

`Session.root_session_id` is self-referential when the session has no `Lineage` parent, and otherwise transitively points to the topmost ancestor. This lets ancestry queries (cost roll-ups, audit aggregation) hit one row instead of recursing.

### Actor-model write serialization

One `AgentActor` per session. All messages targeting the same session (user input, cron triggers, subagent spawns, background-job completions, model re-pins, stop) are queued through the actor handle and consumed sequentially. Therefore `SessionStore` implementations do not need to defend against concurrent writes on the same `session_id` — that guarantee comes from the actor model and is what lets `append_session_message` use a single `INSERT … SELECT MAX(ordinal)+1` without locking. Re-introducing concurrent paths into the session would invalidate the storage contract.

### Source deletion cascades the transcript

`SessionStore::delete(session_id)` runs the `session_messages` cascade and the session-row delete inside one `BEGIN IMMEDIATE` write transaction so a stranded transcript can never outlive its parent.

### Subagent parent deletion drains the subtree first — design intent, not yet wired

`CancelReason::ParentDeleted` exists in `baybo-job` for this, but today `SessionManager::delete` performs no subagent cancellation and has no production caller: the chat `DELETE /v1/chat/sessions/:id` endpoint hides the session via `set_hidden` instead of deleting it. If row-level deletion ever gets a production path, it must trip the subagent's cancellation token (`Cancelled { ParentDeleted }`) **before** the parent row is removed, so a parent never disappears while a child is still running tools or holding LLM state.

### Session ID conventions

`SessionId` is a caller-supplied opaque string. Machine-triggered and spawned producers prefix a UUID v4 so logs and admin listings can recognize them at a glance; user sessions stay bare-UUID:
- TUI / CLI: bare UUID v4 (`SessionId::new()`) — one id per process start (`baybo tui` / `baybo prompt`, overridable with `--session`), plus a fresh one per `/new` conversation. The `cli-` prefix survives only in test fixtures.
- Cron: `cron-<uuid>` — minted fresh by `create_session_with_trigger` on every fire, so each firing gets an isolated session with a clean transcript; continuity across fires is deferred to long-term memory, not a shared session (`Router::handle_cron_trigger`).
- Subagent: minted by `create_spawned_session` as `subagent-<uuid>` — a fresh UUID prefixed by the lineage kind (not derived from the parent's id; the parent link lives in `Lineage`).

`SessionManager::create_session` generates a bare UUID v4 only when no id is requested. A spawned session's `trigger` equals its root session's `trigger` by construction — `create_spawned_session` copies the parent's trigger.

### `SessionState` fields

- `approved_resources`: tool resources the user has granted permanent approval for in this session, populated on each `ApproveAlways` decision. See [`tools.md`](tools.md).
- `background_notifications`: one durable aggregate for background-job notification delivery. `groups` holds barrier cohorts, `buffered_results` holds terminal results not yet committed to the transcript, and `active_delivery` is the retry ledger for the one transcript-backed batch currently being reported. The buffer and active delivery may coexist when fresh jobs finish during an older batch's retry. The aggregate is serde-flattened onto the historical JSON keys, so existing session rows need no migration. See [`background-notifications.md`](../background-notifications.md).
- `subagent_backend`: which backend created this subagent session, plus (for External) the agent's `workspace_dir` and `resume_key`. `None` for non-subagent sessions.
- `subagent_type`: the profile name this subagent session was spawned with, pinned at genesis so a `resume_session_id` call can reject a profile swap. `None` for non-subagent sessions.
- `last_llm`: per-session LLM pin — the `baybo.json` entry name this session's turns resolve against, or `None` to follow `default-llm`. Read by the actor spawner as the loop's `initial_llm`; a live actor is re-pinned via `AgentMessage::SetModel`. Set from the chat UI via `PUT /v1/chat/sessions/:id/model`. See [`agent.md`](agent.md) "Per-session model selection".
- `extra`: reserved extension fields for experimental features or plugin state.

### Idle actor reaping — never row deletion

Session rows and their transcripts are core user data and are **never** dropped by any background sweep (see CLAUDE.md, "Session data is core data — never delete"). Idleness only evicts the in-memory `AgentActor`, never the durable row:

1. The actor reaper (`AgentSupervisor::reap_idle`) periodically calls `SessionManager::idle_sessions(threshold)`.
2. That computes `cutoff = now - threshold` and returns `SessionStore::list_expired(cutoff)` — a list of candidate session IDs and nothing more. The method's doc-comment states it "must never become a delete."
3. For each candidate that has a live registered actor (and no in-flight background subagents), the supervisor sends `AgentMessage::ActorStop`, which trips the actor's cancellation token and lets the registry guard drop the in-memory entry.
4. The session row, transcript, summary cursor, and channel binding all stay live; the next user message re-hydrates a fresh actor from the store. Dropping the actor is therefore lossless.

## Collaboration

| Module    | Role                                                                                                |
| --------- | --------------------------------------------------------------------------------------------------- |
| `model`   | Defines `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`               |
| `storage` | Implements `SessionStore` / `SessionSummaryStore` / `SessionFolderStore` against sqlite; pulls the traits from `baybo-store` (no `baybo-session` dependency) |
| `context` | Owns the in-memory transcript via `ContextManager`; agent loop brokers persistence via this crate  |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance            |
| `cli` / `gateway` | Operator-facing surfaces consume `baybo_agent::SessionManager`                              |
