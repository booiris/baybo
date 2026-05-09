# self_improvement - Memory & Skill Extraction Side-Channel

## Overview

`SelfImprovementManager` is a side-channel agent flow that runs **after** a complex user-chat job completes. It reviews the finished conversation, decides whether anything in it is worth preserving, and — if so — writes new memory entries and/or creates new skills. If nothing in the conversation meets the bar, it writes nothing. The user-facing reply is never blocked by self_improvement.

Lives in `crates/agent/src/self_improvement.rs` — a module of the `agent` crate, not its own crate. The flow itself runs inside a freshly-spawned `TriggerSource::System { reason: SelfImprovement }` session, dispatched through `Router::handle_system_trigger` (the symmetric twin of `handle_cron_trigger`).

Core responsibilities:

- Subscribe to `JobLifecycle::terminal_events` and decide which terminations warrant self_improvement
- Enforce the daily cap, per-user serialization, and global concurrency cap
- Spawn a fresh System session whose `Job.parent_job_id` points back to the originating job (the link the web UI surfaces)
- Bake the originating transcript + identity context into the self_improvement agent's prompt
- Track per-self_improvement cost and abort the agent loop if the per-job cost cap is exceeded

## Design Decisions

### Trigger predicate

A terminal event triggers self_improvement iff:

- `kind == JobStatusKind::Completed`
- `JobKind::UserChat` (excludes `Cron`, `System`, `Spawned`)
- `iterations > config.self_improvement.min_iterations` (default `8`)
- `config.self_improvement.enabled` is true (default `true`)

The `JobKind::UserChat`-only restriction makes the recursion guard structural: a self_improvement Job is `JobKind::System`, so it can never re-trigger self_improvement. No separate guard is needed.

`Failed`/`Cancelled`/`Stuck` terminations are deliberately ignored — half-broken transcripts produce noisy extraction; we want signal from clean completions only.

### Schema additions

- `Job` gains `iterations: u32`. Persisted at job-complete time by `AgentLoop`.
- `JobTerminalEvent` gains `kind: JobKind` and `iterations: u32` so subscribers can filter without re-fetching from `JobStore`.
- `MemoryCategory` becomes `User | Feedback | Project | Reference` (replaces `UserPreference | KeyFact`). One-shot migration: `UserPreference → User`, `KeyFact → User`. The richer category set encodes a real distinction worth having in the schema — *Feedback* in particular is fundamentally different from *Preference* in that it usually has a triggering incident (the "Why") and a scope (the "How to apply").
- `SystemReason::MemoryConsolidation` is renamed to `SelfImprovement` — no callers used the old name.

### Trigger plumbing

`SelfImprovementManager` subscribes to `JobLifecycle::terminal_events` (the same broadcast bus the subagent runtime uses). On a matching event it:

1. Acquires the per-user mutex (see *Concurrency*)
2. Acquires a slot from the global concurrency semaphore
3. Increments and checks the daily-cap counter; on overshoot, drops the event with a `tracing::warn!`
4. Pushes a `SystemTriggerEvent { reason: SelfImprovement, payload }` into a new mpsc owned by `Router`

`Router::handle_system_trigger` mirrors `handle_cron_trigger`: it mints a fresh `TriggerSource::System` session via `SessionManager::create_session_with_trigger`, asks `JobLifecycle` to start a `JobKind::System` Job in that session with `parent_job_id = trigger_job_id`, and dispatches `AgentMessage::SystemTrigger { reason, payload }` followed by `Shutdown` to the one-shot actor. The common spawn boilerplate factors into a private helper shared with the cron path.

`AgentActor` branches on `AgentMessage::SystemTrigger`'s `reason` discriminant, the same way it already branches on `AgentMessage::CronTrigger`'s `TriggerAction`.

### `JobInput::System.payload` shape

```rust
{
    trigger_job_id: JobId,
    originating_user_id: UserId,
    originating_session_id: SessionId,
    iterations: u32,
    retry_count: u8,
}
```

`trigger_job_id` is duplicated with `Job.parent_job_id` deliberately — the payload carries it for symmetry with cron's `action_payload` and so the self_improvement agent can reach it without traversing the parent link.

### Tool ceiling: write + read existing state, separately registered

The self_improvement session's `allowed_tools` is exactly:

| Tool | Purpose |
|---|---|
| `MemoryWrite { user_id, category, content, importance }` | Add a memory entry; routes through `MemoryManager::store_with_dedup` |
| `MemoryList { user_id }` | Paginated read of existing entries — id + content + category |
| `SkillCreate { name, description, body, allowed_tools }` | Write a new `SKILL.md` under `<workspace>/skills/auto/<name>/` |
| `SkillList` | Existing skill names + descriptions |

These four tools are registered through a separate `self_improvement_tools()` constructor and are **never** added to a user-facing agent's tool ceiling. The protection model relies entirely on this registration isolation: the tools all return `vec![]` from `accessed_resources(params)` and therefore bypass the approval gate. If they were ever wired into a normal channel agent's `allowed_tools`, that bypass would be a privilege escalation — the constructor split is the structural guarantee.

The self_improvement agent **cannot** call `UpdateProfile` or otherwise edit identity files (`profile/{SOUL,USER,IDENTITY}.md`). Identity files are the agent's constitution and cascade across every session; one conversation's worth of evidence is too thin a basis for editing them. Identity-level promotion is deliberately *not* automated in v1 — operators do it manually after observing recurring memory patterns.

### Auto-skill governance

Auto-generated skills land at `<workspace>/skills/auto/<name>/SKILL.md` with:

- `Installed` trust level (not `Trusted` — auto-generated content shouldn't get hot-reload + full tool ceiling automatically)
- Hardcoded `disable-model-invocation: true` — the LLM cannot auto-select an auto-skill via the `Skill` tool until a human flips it on. The user can still invoke explicitly via `/<name>`.
- Whatever `allowed-tools` the self_improvement agent writes — no compile-time restriction. Defenses against a subverted prompt creating a dangerous skill are: (i) the trust-level + disable-model-invocation defaults above, (ii) the existing `SkillAssessor` running on first invocation (`Dangerous` blocks per `skills.md`), (iii) the user-facing agent's normal capability ceiling still capping what the union can grant.

The `auto/` namespace gives the operator a single `rm -rf` to undo all auto-generated skills, keeping the human-authored skill directory clean.

### Input shape for the self_improvement agent

The full originating transcript is baked verbatim into the self_improvement agent's initial user message, structured with forgery-resistant envelopes (`<user>`, `<assistant>`, `<tool_call name=…>`, `<tool_result>`) — same idea as `wrap_tool_output_for_llm` in `agent::security`. Specifically:

- **Thinking blocks are included.** They carry the original agent's rationale, which is exactly the kind of pattern worth extracting.
- **Tool results are cropped to 4 KiB each** (tighter than `MAX_TOOL_OUTPUT_BYTES = 32 KiB` because we're concatenating the entire conversation's tool I/O into one prompt). Suffix: `[... truncated for self_improvement, full N bytes]`.
- **Secret placeholders (`[{secret_N}]`) are left intact.** The self_improvement session has no channel and no reveal access, so placeholders never resolve.
- **Total input cap: 80k tokens.** If exceeded, the manager runs one `aura-context` compression pass over the transcript before baking. Most >8-iteration jobs fit under the cap and skip the compression stage.

The originating session's identity files (Soul / USER / IDENTITY) are appended as a labeled "dedup context" block — explicitly framed as *not* behavioral instructions for the self_improvement agent. The prompt warns: "Soul describes how the user-facing agent should behave with the user. Your job is different — extract objective, factually grounded observations. Do not adopt Soul's tone, persona, or stylistic preferences."

### Two-phase prompt structure

The self_improvement agent runs in three phases, structured by the system prompt:

**Phase 1 — Survey.** Call `MemoryList` and `SkillList`. Read the transcript. Produce an internal candidate list — for each candidate, classify as `User | Feedback | Project | Reference | Skill` and write one sentence on its novelty against existing entries.

**Phase 2 — Justify against the bar.** Drop a candidate unless ALL hold:

- (a) Factually grounded — directly traceable to a specific moment in the transcript, not inferred from vibes
- (b) Generalizable — applies beyond this specific job
- (c) Novel — not already covered by an existing entry (paraphrase counts as covered)
- (d) Actionable — would change a future agent's behavior

For `Feedback` and `Project` candidates, REQUIRE a `Why:` line and a `How to apply:` line in the body. Drop the candidate if both can't be written clearly. For `Skill` candidates, REQUIRE concrete recurring procedure (specific tool sequence, specific decision rules); one-off procedures don't become skills.

**Phase 3 — Write.** Call `MemoryWrite` and `SkillCreate` for the survivors. Final assistant message: `"Wrote N memories and M skills. Skipped K candidates: <one-line reasons>."` This message is the `JobOutput`.

`Wrote 0 memories and 0 skills` is a successful terminal state — the "nothing worth saving" outcome is the feature working as intended.

### Two-writer policy with `maybe_store`

The existing `MemoryManager::maybe_store` heuristic remains as a fallback for the simple-conversation path (≤8 iterations, where self_improvement never fires). It is rewritten to emit only the `User` category — `PREFERENCE_INDICATORS` and `FACT_INDICATORS` both map to user-self-description, which fits `User` cleanly. The structured categories (`Feedback`, `Project`, `Reference`) are reserved for self_improvement, which is the only path that can produce the required `Why:` / `How to apply:` body convention.

Two-writer dedup risk is real but bounded: self_improvement has `MemoryList` and is instructed to **supplement, not replace** lower-quality heuristic entries. `MemoryDelete` is intentionally absent from the self_improvement tool ceiling in v1 — a side-channel agent the user never sees deleting memory the user might rely on is the kind of thing that erodes trust the first time it gets it wrong.

### Concurrency

- **Per-user serialization.** `DashMap<UserId, Arc<Mutex<()>>>` — self_improvement acquires the user's mutex before spawning, releases when the System Job terminates. Eliminates shared-memory races and skill-name-collision races between simultaneous self_improvements for the same user.
- **Global concurrency cap.** `Semaphore::new(config.self_improvement.max_concurrent)` (default `8`). Protects LLM-API rate limits and host CPU/memory under burst load (e.g., 50 users finishing complex jobs at the same instant).

Latency is not a concern — self_improvement never blocks a user reply. A queued self_improvement can wait minutes without anyone noticing.

### Daily cap

`config.self_improvement.daily_cap` (default `100`) bounds total self_improvements per UTC day, system-wide (Aura's typical deployment is single-user, so the system-wide cap acts as effectively per-user). Counts **successes only**: failures are wasted spend, not value-producing operations. Manual triggers don't exist (see *Operator surface*) so the cap accounting has no "bypass" mode.

### Failure handling

| Failure mode | Behavior |
|---|---|
| LLM transient error (network, 429, 5xx) | Retry once with backoff; second failure → Job Failed |
| Malformed JSON / schema violation | Job Failed, no retry |
| `MemoryWrite` hits dedup | Silently dropped at `store_with_dedup`; reported in final message as "Skipped 1 duplicate"; counts as success |
| `SkillCreate` name collision | Tool returns error to LLM; LLM retries with `-2`, `-3` suffixes; after 3 collisions, agent skips that skill and continues |
| Iteration cap (6) exceeded | Job Failed, no retry — the prompt is wrong if self_improvement needs >6 iterations |
| Per-job cost cap ($0.50) exceeded | Cancel + Job Failed; protects against runaway thinking models |
| Originating Job data deleted before spawn | Manager checks `JobStore::get(trigger_job_id)`; if gone, drop event silently |

Retry counter lives in `payload.retry_count: u8`; the manager refuses to start a new self_improvement if it would push `retry_count > 1`.

Failed self_improvements land as normal `Job` rows with `JobStatus::Failed { reason }`. No push notification — self_improvement is a side flow the user never opted into seeing per-event. **3 consecutive failures** for the same user within an hour emit a single operator-level `tracing::error!` and persist a `system_health_event` row, catching "the entire self_improvement system is broken" without spamming on incidental flakes.

### Cost integration

SelfImprovement shares CostManager's existing global pool (per `cost.rs:23`, "budgeting is one global pool, `user_id` is provenance not budget dimension"). `Router::handle_system_trigger` runs `cost_manager.check()` pre-flight; on `DailyLimitExceeded` / `MonthlyLimitExceeded`, the System Job is *not* created and the trigger event drops with `tracing::warn!`. The originating user never sees this — self_improvement is best-effort.

The per-self_improvement $0.50 cap and the 100/day count are separate local counters in `SelfImprovementManager`, independent of CostManager. CostManager is the global backstop; the local counters bound self_improvement's worst-case daily spend at $50.

### Operator surface (deliberately minimal)

No new CLI subcommands. No manual trigger entry point. Operators see self_improvements through:

- **Existing `aura jobs list --kind=system`** — the standard job-listing path
- **The `TraceSessionPage` reverse-link** (see *Frontend integration* below)
- **The `AnalyticsPage` activity strip** showing today's `N/100` cap utilization

The only knob exposed to operators is `config.self_improvement.enabled` (whole-system kill switch). No per-user opt-out — defer that until there's a multi-tenant ask. No runtime mutation API.

### Frontend integration

No new web routes. Three additions to existing pages:

- **`JobStore::children(parent_job_id) → Vec<Job>`** new reverse lookup.
- **`TraceSessionPage` for a `UserChat` Job** renders `↘ SelfImprovement (status: completed | failed | running)` in the header when a child self_improvement Job exists, linking to that Job's trace.
- **`TraceSessionPage` for a self_improvement Job** renders `↖ Triggered by user-chat job` in the header, linking back via `parent_job_id`.
- **`AnalyticsPage`** gains a "SelfImprovement activity (last 7 days)" section: bar chart of completed/failed counts by day, plus today's `N/100` cap utilization.

A dedicated `MemoryPage` for browsing/searching/deleting entries is **not** in scope — `aura memory list` covers operator verification needs for v1.

## Constraints

- Lives inside `aura-agent`, not its own crate
- Never blocks a user reply; runs strictly after the originating job's terminal transition
- Triggers only on `Completed` `JobKind::UserChat` jobs (recursion guard is structural, not policy)
- SelfImprovement tool ceiling (`MemoryWrite`, `SkillCreate`, `MemoryList`, `SkillList`) MUST NOT be added to any user-facing agent's `allowed_tools` — protection relies on registration isolation
- SelfImprovement cannot call `UpdateProfile` or edit identity files
- Auto-skills land in `<workspace>/skills/auto/`, `Installed` trust, hardcoded `disable-model-invocation: true`
- Per-self_improvement cost cap and daily count cap are local; global pool is CostManager's

## Collaboration

| Module | Role |
|--------|------|
| `agent::job` | `JobLifecycle::terminal_events` is the trigger source; `JobLifecycle` starts the self_improvement Job |
| `agent::router` | Hosts `handle_system_trigger`; mints the System session; dispatches `AgentMessage::SystemTrigger` to the one-shot actor |
| `agent::session` | `SessionManager::create_session_with_trigger(TriggerSource::System { reason: SelfImprovement })` |
| `agent::memory` | `MemoryManager::store_with_dedup` is the write path; `MemoryList` reads via `MemoryManager::list` |
| `agent::cost` | Pre-flight `cost_manager.check()` in Router; cost provenance recorded under the self_improvement Job's id |
| `agent::context` | Compression pass invoked when the baked transcript exceeds the 80k token cap |
| `skills` | `SkillRegistry` is read by `SkillList`; `SkillCreate` writes new directories that the registry hot-reloads |
| `skills-assessor` | Auto-skills run through the assessor on first invocation (after the operator flips `disable-model-invocation`) |
| `model` | New `MemoryCategory` variants; `SystemReason::SelfImprovement`; `iterations` field on `Job`; `kind`+`iterations` on `JobTerminalEvent` |
| `storage` | Schema migrations for `MemoryCategory` enum, `iterations` column on `jobs`, `system_health_event` table |
| `tools` | `MemoryWrite` / `SkillCreate` / `MemoryList` / `SkillList` registered via a dedicated `self_improvement_tools()` constructor |
