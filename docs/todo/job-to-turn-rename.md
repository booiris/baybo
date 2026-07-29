# Rename the `Job` domain entity to `Turn`

The turn-entity `Job` — `baybo_job::Job`, `JobId`, the `jobs` table, the tier
between `Session` and `Step` in the trace hierarchy — becomes `Turn`. The word
"job" survives in the repo only where it already means something else.

## Why

Three unrelated concepts share the word today: the turn entity, **background
jobs** (detached bash + background subagents, `/v1/background-jobs`), and **cron
jobs**. A fourth, `store::AssessmentJob`, is prefix-qualified. Only the turn
entity is the bare word `Job` — and it is the one the client-facing vocabulary
already calls a *turn* (`TurnSnapshot`, `active_turn_started_at`, the turn-state
projector, `Frame::Status`). The rename aligns the internal name with the wire
and frees "job" to mean exactly one thing at every remaining site.

## Decisions

| # | Decision |
| --- | --- |
| 1 | Full domain rename — crate, types, SQL tables, REST paths, web. Not a trace-only relabel. |
| 2 | `Job::is_turn()` → `Turn::is_chat_turn()`. |
| 3 | Existing rows are migrated in place, one-time and idempotent, on the `init_db` owner-channel precedent. |
| 4 | Pre-rename bench `trace.json` artifacts are expendable — no dual-read. |
| 5 | `channels::TurnStatus` → `StatusPhase`, freeing `TurnStatus` for the lifecycle enum. |
| 6 | Compiler-driven rename. No committed rename script. |
| 7 | Migration is one-way. Rollback = restore the pre-deploy snapshot. |
| 8 | Trace viewer relabels only; the turn-kind badge is a follow-up. |
| 9 | Acceptance includes a migration dry-run against a copy of a real DB. |
| 10 | Trace response body keys rename, guarded by an extended `web_trace_types_sync`. |

### Turn vs chat turn

`Turn` is the **row**: every externally-triggered unit of work, including
`/compact` and cron-result delivery. A **chat turn** is the subset the user
sees — what drives `TurnState`, what `/stop` cancels, what crash recovery
closes per-session. `Turn::is_chat_turn()` is the only predicate for that
subset; `kind.rs` already uses the phrase ("it is not a chat turn and must not
drive TurnState or push notifications").

The two must stay distinguishable in prose. `docs/modules/turn.md` carries the
entity definition; `docs/CONTEXT.md`'s existing **Turn identity** entry gains a
cross-reference, because after this rename "Turn" is ambiguous for clients too.

`is_chat_turn` is **not** "produces a reply": `CronNotification` produces a
durable reply row and drives push, yet is excluded (it runs no inference). It is
also not "drives push": push's set is `{UserChat, Cron, CronNotification}`,
which includes what `is_chat_turn` excludes. The three subsets **cross**; do not
collapse them into one predicate.

## Naming

| From | To |
| --- | --- |
| `baybo-job`, `crates/job/` | `baybo-turn`, `crates/turn/` |
| `Job` / `JobId` | `Turn` / `TurnId` |
| `JobStatus` / `JobStatusKind` | `TurnStatus` / `TurnStatusKind` |
| `JobPhase` | `TurnPhase` |
| `JobInput` / `JobInputKind` / `JobOutput` | `TurnInput` / `TurnInputKind` / `TurnOutput` |
| `JobLifecycle` / `JobLifecycleEvent` | `TurnLifecycle` / `TurnLifecycleEvent` |
| `JobStore` / `JobRow` / `SessionJobStats` | `TurnStore` / `TurnRow` / `SessionTurnStats` |
| `JobError` / `JobTransition` | `TurnError` / `TurnTransition` |
| `JobCancellationRegistry` / `Guard` | `TurnCancellationRegistry` / `Guard` |
| `Job::is_turn` | `Turn::is_chat_turn` |
| `list_active_turns_by_session` | `list_active_chat_turns_by_session` |
| `parent_job_id` | `parent_turn_id` |
| `channels::TurnStatus` | `StatusPhase` |
| `docs/modules/job.md` | `docs/modules/turn.md` |

`CancelReason` and `TriggerKind` keep their names. Every persisted **value**
stays byte-identical — `TurnStatusKind::as_snake_case()` still yields
`pending`/`in_progress`/…, and `turn_input_kind_str` still yields
`user_chat`/`cron`/`compact`/… Only identifiers move.

`channels::TurnStatus` → `StatusPhase` is free: it is presentation-only and
flattens to `"compacting"`/`"compacted"` on the wire, so no client sees a
change. Its own doc already calls it a turn-**phase** signal and it lands in a
field named `phase`.

### Not renamed

`BackgroundJob*` / `/v1/background-jobs` / `JobList` / `JobStop` /
`app/web/src/pages/JobsPage.tsx` and the rail's "Jobs" entry · `cron_jobs`,
`cron_executions.job_id`, `CronJob*`, `cron_job_id`, `JobIdParams`
(`crates/cron/src/tools.rs:471`) · `store::AssessmentJob*` ·
`skill_risk_assessment_jobs` · GitHub Actions "job" · the English idiom
("substitution is the caller's job").

## Data migration

One-time, idempotent, in `init_db` after the DDL — the shape
`sqlite/mod.rs:1058` already uses for the owner-channel collapse. Each step
guards on `pragma_table_info` so a second pass is a no-op.

**1. `jobs` → `turns`.** `ALTER TABLE jobs RENAME TO turns`, then
`RENAME COLUMN parent_job_id TO parent_turn_id`, then rewrite the blob:
`data = json_set(json_remove(data,'$.parent_job_id'), '$.parent_turn_id', …)`
where the old key is present. `Turn.parent_turn_id` is `Option` **without**
`#[serde(default)]` (`crates/job/src/lib.rs:165`) — confirm during the dry-run
whether a missing key errors or silently reads `None`. If it is silent, the
next status transition writes the null back and the subagent hierarchy is
destroyed permanently.

**2. `sessions.data` lineage.** `Lineage.parent_job_id` is a **required** serde
field (`crates/model/src/session.rs:201`) inside the session blob, and
`decode_session_list_rows` (`sqlite/session.rs:146-160`) **skips** any row that
fails to deserialize with only a `warn!`. Miss this and every spawned/subagent
session silently disappears from the chat list — a core-data invariant.
Rewrite `$.lineage.parent_job_id` → `$.lineage.parent_turn_id`, and
`ALTER TABLE sessions RENAME COLUMN parent_job_id TO parent_turn_id`
(`mod.rs:479`; the column is write-only — the live read path is the blob).

**3. `cost_records`.** `RENAME COLUMN job_id TO turn_id` (`mod.rs:717`). Flat
column, no blob. Cost write failures are swallowed by a `warn!`
(`crates/cost/src/manager.rs:461`), so a DDL/DB mismatch here stops billing
silently rather than erroring.

**4. `steps` rebuild.** `job_id` is
`GENERATED ALWAYS AS (json_extract(data,'$.job_id')) VIRTUAL` (`mod.rs:776`).
SQLite cannot alter a generated column's expression, and `RENAME COLUMN`
succeeds on it *without* rewriting the expression — which is the trap. Rebuild
inside a transaction: create `steps_new` with
`turn_id … AS (json_extract(data,'$.turn_id'))`, `INSERT … SELECT id,
json_set(json_remove(data,'$.job_id'),'$.turn_id',json_extract(data,'$.job_id'))`,
drop, rename, recreate `idx_steps_turn` and `idx_steps_open`.
`crates/trace/src/step.rs:25-29` already documents this coupling as
load-bearing.

**5. Index names.** SQLite carries indexes across `ALTER TABLE RENAME` but keeps
their old names, so `CREATE INDEX IF NOT EXISTS idx_turns_*` would build
duplicates. Explicitly `DROP INDEX` `idx_jobs_session`, `idx_jobs_status`,
`idx_jobs_created`, `idx_jobs_parent`, `idx_cost_job`, `idx_steps_job`.

**6. Guard on `pragma_table_xinfo`, never `pragma_table_info`.** `table_info`
**omits VIRTUAL generated columns entirely** — on a pre-rename DB it reports
`steps` as having only `(id, data)`. A guard written against it reads
`steps.job_id` as absent, skips the rebuild, and the DDL then fails creating
`idx_steps_turn` on a column that was never added. Only `table_xinfo` lists
generated columns. This cost a full dry-run cycle to find and is invisible on a
fresh database.

### Rollback

One-way by decision. A binary rolled back past this commit re-creates an
**empty** `jobs` table via `CREATE TABLE IF NOT EXISTS` and boots clean
reporting zero history, while trace queries hard-fail on the missing
`steps.job_id`; rolling forward again then dies renaming `jobs` onto an
existing `turns`. Rollback is therefore "restore the pre-deploy snapshot",
which the `dylan` redeploy procedure already takes.

## Surfaces

**Rust.** ~1 551 identifier sites, compiler-verified once the types move.
`crates/job` → `crates/turn` needs a `git mv` plus the root `Cargo.toml` member
path and workspace dep path; 9 `Cargo.toml` files name `baybo-job` (root +
agent, baybo, cli, gateway, integration-tests, query, storage, trace, and the
crate itself). `Cargo.lock` carries 9 entries and must be regenerated and
committed.

**Compile-invisible Rust sites** — none of these fail to build:

- `crates/llm/src/billed.rs:37` `Attribution.job_id`, populated by *field
  shorthand* at four call sites (compression, progress_observer, title,
  tool_executor) — a partial rename compiles. `billed.rs:55` mints a synthetic
  id for unattributed calls; as `TurnId::new()` it asserts a turn that never
  existed. Consider naming that path explicitly rather than leaving it implied.
- **`BackgroundJobKind`'s serde tag** (`crates/model/src/spawn_protocol.rs`) is
  `#[serde(tag = "job")]` — a *background* job, outside the rename, but a
  persisted discriminator. It rides inside
  `Session.background_notifications.pending_background_results`, so retagging it
  to `turn` makes any session holding a buffered background result fail to
  deserialize **whole**, and `decode_session_list_rows` drops such rows with only
  a `warn!` — a live conversation silently leaves the user's list. A sweep that
  rewrites the bare token `job` hits this; the compiler cannot. Pinned by
  `background_job_kind_keeps_its_persisted_tag` against a literal pre-rename
  blob (a round-trip test agrees with itself and proves nothing).
  A migration dry-run does **not** catch this: it only fires on sessions
  persisted while a background result was buffered, and a sampled database
  usually has none.
- `crates/cli/src/cli.rs:872,875` `conflicts_with_all = ["session", "job"]` —
  clap resolves arg ids by string at `Command`-build time, so renaming the field
  without these **panics on every CLI invocation**.
- `"cron_job_id"` written as a literal key into `TurnInput::Cron.action_payload`
  (`crates/agent/src/actor/mod.rs:632`, read at `agent_loop.rs:427`) — cron
  sense inside a turn-entity blob. A `job_id` sweep breaks cron prompt
  reconstruction with no signal.
- `cohort_key` → `"{turn_id}::{group}"` (`crates/model/src/session.rs:531`) is a
  persisted map key under `background_groups`. The interpolated value is
  rename-safe; do not "tidy" the format or in-flight cohorts strand.
- Operator-visible text moves with the rename: 12 `jobs.*` op labels in
  `sqlite/job.rs`, `"job not found: {0}"` / `"job storage error: {0}"`
  (`job/src/error.rs`), `query/src/lib.rs:37,494,1152`, and the
  `job = %ev.job_id` tracing field in `gateway/src/push/mod.rs` that lands in
  the `/v1/logs` ring buffer.
- ~845 Rust comment lines carry a bare "job", a large share of them cron or
  background sense. With no rename script, these are a manual triage; a
  `rg -inw 'jobs?|job_id'` sweep at the end should leave only the
  **Not renamed** buckets above.

**REST.** `/v1/jobs`, `/v1/jobs/{id}`, `/v1/jobs/{id}/cancel`, and
`/v1/traces/{session_id}/jobs/{job_id}` → `turns`. Clean break, no aliases: a
bearer-gated admin API whose only client is the dashboard embedded in the same
binary. `/v1/background-jobs` is untouched. Body keys rename too — `"jobs"`,
`"job_id"`, `"job_status_kind"` at `traces.rs:128-130,145,190-192` — and these
are hand-written `json!` literals against untyped `serde_json::Value` responses,
so nothing but the new gate will catch a miss. `docs/openapi.json` embeds
`baybo_job::Job` in description strings, so the drift gate trips on the crate
rename alone; regenerate with
`UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync`
then `pnpm gen:api`. `app/web/src/api/schema.d.ts` has **no** CI drift gate.

**CLI.** `baybo job list|show|cancel` → `baybo turn …`, and
`crates/skills/src/builtin/baybo-cli/SKILL.md:48-50` in the same commit — it
hardcodes the verbs and is injected into skill selection, so a stale skill makes
the model emit a dead command.

**Web.** Rename `TraceJobSummary`/`JobTrace`/`JobStatusKind`/`job_id` in the
hand-written `app/web/src/types/trace.ts`; `JobAnchors.tsx` → `TurnAnchors.tsx`;
`data-job-id` → `data-turn-id`; `?job=` → `?turn=` (breaks existing bookmarks —
acceptable) noting the writes are **property shorthand** at
`TraceSessionPage.tsx:1297,1314,1318`, so grepping `'job'` finds only the read.
Visible strings `Job #N` → `Turn #N`, the `jobs` stat label, `no jobs`, and
`{n} job|jobs`. `JobsPage.tsx` and the rail's "Jobs" stay — background jobs.

**iOS — no change.** Verified: `app/ios/ffi/src/gateway_api.rs:13-18` reaches
only `/v1/chat/*`, `/v1/cron`, `/v1/llm/models`, `/v1/deck`,
`/v1/mobile/apns-token`, `/v1/blobs` — never `/v1/jobs` or `/v1/traces`. There
is no trace UI on iOS, no FFI DTO or persisted mirror field carries a turn-entity
id, and every "job" in `app/ios` is a cron job. The sole exception is one stale
doc comment at `app/ios/web/src/Transcript.tsx:2495`.

**bench.** `bench/bench-web/src/trace.rs` and its own hand-written
`bench/bench-web/web/src/types/trace.ts` rename with everything else; pre-rename
`trace.json` artifacts stop rendering, by decision. Note that bench-web's copy of
the trace types is **already divergent** from `app/web`'s and is covered by no
gate. `bench/*/run.sh` parses `"jobs"` inside a bare `except Exception: pass`, so
it degrades to zero rather than failing.

**Docs.** `docs/modules/job.md` → `turn.md`, rewritten rather than sed'd; update
the `docs/modules/README.md` index and `docs/modules/trace.md`'s hierarchy line
to `Session > Turn > Step > Span`. ~371 markdown hits, mixed senses. Also
`crates/workspace/src/singleton.rs:3,5` and `crates/security/src/key_file.rs:167`
carry turn-entity prose; `crates/sandbox/src/lib.rs:89` and
`sidecars/channel/telegram/src/platform.ts:31,96` do not.

## Acceptance

Draft PR (`gh pr create --draft`), never marked ready. A draft runs **no** CI
and `gh pr checks` reports `skipping` with exit 0 — indistinguishable from
green — and Actions is billing-blocked regardless, so local gates are the only
signal this change will get.

- `cargo fmt`
- `cargo clippy --all --benches --tests --examples --all-features` — zero warnings
- `cargo nextest run --workspace` — no `--all-features`
- `scripts/check-ts-bindings.sh`, `pnpm --filter @baybo/channel-sdk test`
- OpenAPI regen → `pnpm gen:api` → web `vite build`
- `web_trace_types_sync` extended to assert `turn_id`/`turn_status_kind` are
  present in `trace.ts` **and** `job_id` is absent
- `rg -inw 'jobs?|job_id'` residue sweep, every hit triaged into a
  **Not renamed** bucket

**Migration dry-run** against a copy of a real DB (`/data/aura/.baybo/state/`
holds 738 jobs / 6 863 steps / 7 994 cost_records), asserting:

1. `count(*)` on `turns` and `steps` unchanged;
2. `select count(*) from steps where turn_id is null` = **0** — the failure this
   whole plan hinges on, whose symptom is an empty trace tree with no error;
3. `turns.parent_turn_id` non-null count matches the pre-migration
   `parent_job_id` count — catches the silent-`None` drop;
4. spawned/subagent sessions still appear in the chat list — catches the
   `Lineage` row-skip;
5. the trace viewer renders a pre-rename session.

## Follow-ups

- **Turn-kind badge.** `TraceTurnSummary` carries no input kind, so the viewer
  cannot tell a `UserChat` turn from a `/compact`. A session with two messages
  and one compaction renders "Turn #1, #2, #3" and disagrees with the chat UI.
  Adding `turn_input_kind` through `baybo_query` → REST → OpenAPI → TS is a
  feature, deliberately not in this rename.
- **bench-web trace-type drift.** Its mirror is unguarded and already stale.
- **`schema.d.ts` drift gate.** Nothing checks the generated client.
- **Recovery asymmetry.** `recovery.rs:98` sweeps every non-terminal turn at
  boot; `recovery.rs:185` sweeps only chat turns on actor panic. An orphaned
  `/compact` is closed by the former and not the latter. Legible today as "turns
  vs all jobs"; after the rename only `is_chat_turn` marks it.
