# subagent - Typed Subagents and the spawn_subagent Tool

## Overview

The `subagent` crate (`baybo-subagent`) owns the typed-subagent domain end to end: the `SubagentProfile` definition, the process-wide `SubagentRegistry`, the per-root fan-out `SubagentDispatchLimiter`, and the `spawn_subagent` `Tool` itself. It mirrors `baybo-skills` and `baybo-cron` — a domain crate hosts its own `Tool` impl and depends on `baybo-tools` for the trait, and `baybo-tools` never depends back (that would be a cycle).

A `SubagentProfile` declares the `system_prompt` + default model tier for a single `subagent_type` value the parent LLM can emit when calling `spawn_subagent`. The profile's `system_prompt` **fully replaces the parent's Soul** for the spawned child actor — the profile author owns the child's identity, security, and output contracts; base Soul is not threaded through.

The spawn protocol types (`SubagentSpawnRequest`, `SubagentResult`, `SubagentBackend`, `SubagentParentContext`, `SPAWN_SUBAGENT_TOOL_NAME`) live in `baybo-model`; this crate consumes them. The `SubagentSpawner` capability trait lives here in `baybo-subagent` (leaf crate, no cycle); its actor-backed impl is in `baybo-agent`. Profiles are loaded from a single `<workspace>/agents/<name>.md` markdown file (frontmatter + system-prompt body).

### Public surface

- **`SubagentProfile`** — `name`, `version`, `description`, `system_prompt`, `default_tier: Option<ModelTier>`, `source: ArtifactSource`, `trust_level: TrustLevel`, `source_path: Option<PathBuf>`, `tools: Option<Vec<String>>`.
- **`SubagentProfileSummary`** — hot-path projection (`name` + `description` + `default_tier`) used by the `spawn_subagent` description renderer that runs every LLM turn; skips cloning the `system_prompt` body.
- **`SubagentRegistry`** — process-wide profile lookup (interior mutability via `DashMap`, an `AtomicU64` `version()` for cache invalidation).
- **`tool::SpawnSubagentTool`** + **`tool::SpawnSubagentToolConfig`** + **`tool::make()`** — the `spawn_subagent` `Tool` (tool name = `baybo_model::SPAWN_SUBAGENT_TOOL_NAME`). In the public `tool` module, not re-exported at the crate root.
- **`SubagentDispatchLimiter`** trait + **`FanOutLimiter`** (production) + **`unbounded_limiter()`** (returns the crate-private unbounded stub for test/non-production wiring).
- **`load_profile_from_file`** / **`parse_profile_md`** (loader) and **`validate_profile_name`** / **`validate_profile_version`** / **`normalize_line_endings`** (validation).

## Design Decisions

### The profile replaces Soul, it does not layer on it

A child actor boots with an empty context, the profile's `system_prompt` as its entire system contract, and the self-contained `prompt` the parent authored as its first user message. The child does **not** see the parent transcript. This keeps subagents predictable and auditable — what a profile declares is exactly what the child runs.

### Registry mirrors SkillRegistry

`SubagentRegistry` follows the same pattern as `baybo-skills::SkillRegistry`:

- `register` overwrites by name; `register_builtins` populates the bundled profiles so a fresh workspace boots with at least the catch-all `general-purpose` target.
- `load_dir` remembers the directory so `reload` can replay disk state. `reload` is **authoritative-disk**: it clears the map and re-scans every remembered directory, so profiles deleted from disk drop out and programmatic `register` calls without a backing file are cleared. Built-ins are *not* auto-re-registered on `reload` — the caller drives `register_builtins` ↔ `reload` ordering explicitly.
- `version()` is a monotonic counter bumped on every mutation. The `spawn_subagent` tool's description (which the LLM sees every turn) caches its rendered catalogue against this version and rebuilds only on mismatch.
- `all_summaries_sorted` is the lightweight, hot-path listing (no `system_prompt` clone); `all_sorted` is the heavy full-profile variant.

### spawn_subagent is a blocking dispatch tool

`SpawnSubagentTool::execute`:

1. Resolves `subagent_type` against the registry — an unknown type returns a hard `ToolError::InvalidParams` listing the catalogue (no soft fallback).
2. Walks the parent's lineage via `SessionManager` to compute depth (the root session id is read off the denormalized `Session.root_session_id`; depth still needs the walk, capped at `MAX_LINEAGE_WALK_HOPS = 128` against corrupt chains). Depth `>= max_depth` → `ToolError::SubagentDepthExceeded`.
3. Reserves a fan-out slot under the root via the dispatch limiter **before** shipping. Over cap → `ToolError::SubagentFanoutExceeded`.
4. Hands the `SubagentSpawnRequest` (plus a `SubagentParentContext` carrying the parent's session/turn/span ids, cancel token, the turn's `background_eligible` gate, and any transient `InheritedToolContext`) to the actor-backed `SubagentSpawner` — reached via a late-set slot, since the tool is built before the spawner exists — and returns the `SubagentResult`: the child's terminal for a foreground spawn, or the dispatch ack for a background one.

When a Baybo child is spawned from an execution carrying an
`InheritedToolContext`, the tool forwards `ToolContext::inherited_context`
unchanged and the actor-backed spawner installs it on the child's `Spawned`
turn. Descendants repeat the same propagation, without knowing whether cron or
another dispatcher created the context. This is execution context, not session
identity: it is not stored on the child row, an independently resumed child does
not recover it, and `Some(default())` deliberately keeps the child fail-closed.
External backends do not consume it because they do not run through Baybo's MCP
registry or `ToolExecutor`.

The tool is registered by the runtime wiring code (`crates/baybo/src/runtime.rs`), **not** by `baybo_tools::builtin::default_tools`, because it needs the runtime-owned spawner slot and the live `SubagentRegistry` for its per-turn description. Its manifest carries `TrustLevel::Trusted` and an empty capability set.

**Caps** (constructor-overridable defaults): `DEFAULT_MAX_SUBAGENT_DEPTH = 3` bounds the lineage chain; `DEFAULT_MAX_SUBAGENTS_PER_ROOT = 8` bounds concurrent breadth under one root. The tool requires a `max_timeout`, so it uses a 30-day `TOOL_WAIT_BACKSTOP` that never fires in practice — subagent execution is no longer wall-clock-bounded (baybo subagents stop at `max_iterations`, external ones at their own internal safety timeout).

**Tool parameters** (`subagent_type`, `description`, `prompt` required):

- `backend` — `"baybo"` (default, full in-process baybo agent) or `"claude"` / `"codex"` (one-shot external CLI backends; see [`../external-agents.md`](../external-agents.md)). Parsed via `SubagentBackend` / `ExternalAgentKind`.
- `model_tier` — `default` / `lite` / `balanced` / `deep` (`fast` is still accepted as the pre-rename spelling of `lite`); `default` is the strict-schema no-op and preserves the precedence profile `default_tier` > pool default. An explicit non-default tier wins. Only applies to `backend="baybo"`.
- `background` — when `true`, returns a dispatch ack immediately and surfaces the child's final result as an out-of-band notification on the parent's next turn.
- `on_timeout` — `"background"` (default) or `"kill"`: what to do when a foreground spawn is still running after the fixed 2-minute foreground wait (`SUBAGENT_FOREGROUND_WAIT`). `background` detaches it (handle now, notification on terminal); `kill` cancels it, surfacing `SubagentExitStatus::ForegroundWaitElapsed` — distinct from `Timeout`, which means the backend emitted nothing at all. Both render the child's transcript path so the parent can read the work the kill interrupted instead of re-running it. Ignored when `background=true`, and on a turn that is not `background_eligible` (see [`agent.md`](agent.md) → *Background jobs*) — there the spawn blocks until the child is terminal, with no foreground-wait timer at all. Parsed via `OnTimeout`.
- `group` — barrier cohort name. Subagents sharing a `group` (spawned together in one turn) are forced background-from-start and deliver a single merged notification only once they all finish; the spawner namespaces the cohort by the dispatching turn's id (`BackgroundNotificationGroup::cohort_key`).
- `resume_session_id` — continue a prior child's conversation (foreground results carry a `[subagent_session_id: …]` tail the parent can pass back).

### Fan-out limiter is a per-root capacity gate

The fan-out cap is enforced **outside** the depth check: depth bounds chain length, fan-out bounds concurrent breadth under one root session. The `SubagentDispatchLimiter` trait (`try_reserve(root, cap) -> Result<(), current_count>`, `release(root)`, `in_flight(root)`, `snapshot()`) lives in this leaf crate so both consumers share one definition without a cycle: the `spawn_subagent` tool reserves a slot, and `baybo-agent`'s `ActorSubagentSpawner` (`runtime/subagent_spawner.rs`) releases it on the child's terminal event. `FanOutLimiter` backs this with a sharded `DashMap<SessionId, u32>` whose entry API gives an atomic-within-shard check-and-increment (two parallel spawns under the same root cannot both observe `cap - 1`). `release` decrements-or-removes while holding the shard guard so it can't race a re-increment. The lifecycle contract is: one successful `try_reserve` ⇒ exactly one eventual `release` (converging on the spawner across its setup-failure paths — parent-session load, child-session resolution — and the wait task's terminal hook; the tool itself releases only on the unwired-spawner fallback, where `spawn` is never reached).

### Profile format: a single `<name>.md`

One file per profile under `<workspace>/agents/` — no directory-per-profile ceremony like skills, because a profile has no linked-files concern (the prompt body *is* the whole thing). The loader accepts a narrow YAML subset of its own (flow scalars, optionally quoted; `|` literal blocks for multi-paragraph descriptions — unlike `baybo-skills::loader`, which takes bools and inline/block lists but no literal blocks) and **rejects** anything more exotic — inline/block lists, bools, nested maps, anchors — so misreads can't slip through. `name` defaults to the filename stem; `description` is required (it's shown to the parent LLM when picking a type); `version` defaults to `0.0.0`; `default_tier` is optional. `tools` is optional and comma-separated (`tools: Read, Grep, Glob`): it names the only tools a child on this profile is offered, intersected with the channel and trigger axes in [`tools.md`](tools.md). Absent means unrestricted; present-but-empty is a load error, because a profile that meant "no tools" would otherwise be indistinguishable from one that forgot. It is a prefix budget rather than a gate — the executor does not refuse an unlisted name — so a list has to cover everything the profile body tells the child to do. `description` is the one line the profile gets in the `spawn_subagent` catalogue the parent LLM picks from, so write it as "what I am, and when to pick this over the neighbouring profiles" — the second half is the only question the catalogue is asked. The budget is `MAX_CATALOGUE_BLURB_CHARS`; a description that fits is rendered whole, and one that does not falls back to its first sentence rather than a hard cut, because cutting at the budget removed exactly the disambiguating clause from all four built-ins. `explorer`, `planner` and `reviewer` name read-only lists (~12 tools instead of the ~32 a board subagent would otherwise carry); `general-purpose` names none. The resolved list rides `SubagentSpawnRequest::tool_allowlist` and is pinned onto the child's `SessionState` beside `subagent_type`, so a later edit to the profile file reaches the next child rather than the one already running. The body after the frontmatter becomes `system_prompt` (leading/trailing blank lines trimmed) and cannot be empty.

`validate_profile_name` (`^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$`) and `validate_profile_version` (`^[a-zA-Z0-9._\-+~]{1,32}$`) reject whitespace, quotes, and angle brackets at load time — frontmatter is untrusted input that could otherwise try to forge a trust level by breaking out of the XML/JSON envelope the runtime renders profiles into.

### Built-in profiles

Four profiles are compiled into the binary via `include_str!` (`builtin/*.md`) and surfaced through `builtin::all()` → `register_builtins`: **`general-purpose`** (the catch-all), **`explorer`** (default tier `lite`), **`planner`** and **`reviewer`** (default tier `deep`). Built-ins are stamped `ArtifactSource::Inline` / `TrustLevel::Trusted`; a bundled profile that fails to parse logs an error and is skipped rather than crashing boot. Workspace profiles registered later with the same name override a built-in.

## Constraints

- Internal deps: `baybo-model` (spawn protocol + domain types), `baybo-session` (lineage walk for the depth check), `baybo-tools` (the `Tool` trait). **No** dependency on `baybo-agent` or `baybo-context` — those depend on this crate, never the reverse.
- Mirrors `baybo-skills` / `baybo-cron`: a domain crate owning its own `Tool` is only acyclic because `baybo-tools`'s dependency graph never reaches back here.
- The crate is pure domain + tool logic; it persists nothing itself. Profile discovery is filesystem-only (`<workspace>/agents/`); there is no `SubagentStore`.

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `crates/baybo/src/runtime.rs` constructs the `SubagentRegistry` (`new` → `register_builtins` → `load_dir(workspace_paths.agents_dir())`), the `FanOutLimiter`, and the `spawn_subagent` tool via `tool::make`. The `runtime::subagent_spawner::ActorSubagentSpawner` (with the wait routine in `actor/subagent.rs`) builds/links the child actor, carries any transient `InheritedToolContext` into its spawned turn, and releases the fan-out slot on the child's terminal event |
| `context` | `ContextManager` holds an optional `(Arc<SubagentRegistry>, subagent_type)`; on seed it resolves the type back to the profile's `system_prompt` and uses it as the child's system row in place of Soul |
| `model` | Owns `InheritedToolContext`, the spawn protocol (`SubagentSpawnRequest` / `SubagentResult` / `SubagentBackend` / `SPAWN_SUBAGENT_TOOL_NAME`), plus `ModelTier`, `ArtifactSource`, `TrustLevel` |
| `tools` | Provides the `Tool` trait + `ToolContext` / `ToolManifest` the `spawn_subagent` tool implements |
| `session` | `SessionManager` backs the lineage walk that powers the depth cap |
| `workspace` | `WorkspacePaths::agents_dir()` resolves the `<workspace>/agents/` profile directory (a standalone git repo, created and `git init`-ed by `ensure_layout`) |

External (`claude` / `codex`) subagent backends are documented in [`../external-agents.md`](../external-agents.md).

Reading a child session back — the three GET-only `/v1/chat/subagents` routes,
the lineage-scoped readability predicate they admit on, and why the child
transcript needs `seed_as_user` to have a visible task at all — is documented in
[`../../app/ios/docs/subagents.md`](../../app/ios/docs/subagents.md). Note that
`resolve_child_session` stamps `child.title = task_summary` for that surface: it
is the only place the task text reaches the session row.
