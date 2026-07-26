# TRACE Detail — Unified Tree Redesign

## Problem

The trace-detail view (`app/web/src/pages/TraceSessionPage.tsx`, ~1800 lines) is a
three-pane layout:

- **Left, 120px `JobSidebar`** — a flat list of jobs (`#1 #2 #3…`) carrying only
  status + token chips + interjection count.
- **Middle `StepBlock` card stream** — each step is a `3px` bordered, `shadow-brutal`
  card with a header row and nested `SpanRow`s; one step easily costs 100–200px.
- **Right, 480px** — `SpanDetailPanel` (I/O · Meta · Events) for a selected span, or
  `JobSummaryPanel` when nothing is selected.

Three pain points surface once a session gets complex:

1. **Job navigation doesn't scale.** The narrow flat `#N` list gives no way to tell
   *which* job holds the problem, and no structure to drill through.
2. **Low information density.** The heavy step cards mean only a few steps fit on
   screen; a job with dozens of steps is all scrolling.
3. **Subagents are disconnected.** A subagent runs in a **separate session**
   (`child_session_id` on the parent's `subagent` step / `subagent_stub` span). Today
   drilling in calls `navigate('/traces/{child}')` — it jumps away and loses the parent
   context. There is no way to see parent and child together.

## Target design — unified tree (2 panes)

Replace the flat `JobSidebar` **and** the middle `StepBlock` stream with a single
**collapsible tree navigator** (left) plus a **contextual detail panel** (right).

```
┌─────────────────────────────┬──────────────────────┐
│ unified tree (collapsible)  │  detail of selection  │
│ ▸ Job #1  ok  12s           │  span → I/O·Meta·Evt  │
│ ▾ Job #2  ● 1 FAIL          │  Step → outcome +     │
│    ▸ LLM iter ok            │         failure + span│
│    ▾ LLM iter ● FAIL        │  Job  → in/out/token  │
│       ▸ Tool bash ● FAIL ◀  │  subagent → child     │
│    ▾ ⧉ Subagent→child ●FAIL │      overview + Task  │
│       ▾ [child] Job#1 ●FAIL │      prompt + open    │
│ ▸ Job #3  ● 2 FAIL          │  ┌ breadcrumb ─────┐  │
│ [全部|仅失败|按kind|搜索…]  │  root›#2›iter›bash    │
└─────────────────────────────┴──────────────────────┘
```

**Hierarchy.** `Session → Job → Step → Span`, matching `docs/modules/trace.md`
(`Session > Job > Step > Span (+ SpanEvent)`, a fixed three-layer fan-out; steps and
spans do not nest among themselves). A subagent child session hangs under the parent
span that spawned it (`Lineage.parent_span_id`), recursively.

### Locating the problem (pain 1)

- On load, collapse everything to job level, but **auto-expand the ancestor chain of
  every `failed` / `stuck` / `pending` node** so the problem is visible immediately.
- Collapsed parents carry a **roll-up badge** — a red dot + failure count of their
  subtree.
- A URL-selected node auto-expands its ancestors.
- A **filter bar** at the top of the tree: a text search (kind / tool / model) + a
  **Failures only** toggle. When any filter is active, every job's step tree is
  eager-loaded so the filter never silently hides a matching-but-unloaded job.

### Density (pain 2)

- A step collapses to a **single row**: icon + kind label + one-line summary (reuse
  `stepSummaryText`) + outcome badge + duration. Expand to reveal its span rows.
- `parallel_group` siblings keep their `‖ parallel` marker.
- Failed / selected nodes auto-expand; everything else collapses.

### Subagents (pain 3)

- One up-front call fetches the whole lineage tree + each descendant session's job
  summaries (status/tokens) so **failure badges and auto-expand cross the subagent
  boundary** — you see "this subagent contains a failure" and it opens down to the
  child's failing region.
- The heavy per-job step/span tree stays **lazy** — fetched only when a specific step
  region is expanded.
- A child session renders under its spawning span with a session-boundary marker
  (`⧉ Subagent → child …`), indented; its own `Job → Step → Span` nests beneath.
  Recursion is bounded by the existing `lineage_tree` `MAX_DEPTH = 32`.

### Interaction & the detail panel

- **Chevron toggles expand; clicking the rest of the row selects** — decoupled.
- Every node is selectable; the right panel is contextual:
  - **span** → existing `SpanDetailPanel` (I/O · Meta · Events).
  - **Step** → a new step panel: outcome + failure reason + a summary of its spans. A
    single-span step (compression / memory / skill / subagent) deep-links straight to
    that span's detail.
  - **Job / child session** → existing `JobSummaryPanel` (input / output / tokens /
    activity).
  - **subagent container** → child job overview + the spawn context (the `Task`/spawn
    tool call's prompt & params) + an "open child as full page" link (keeps today's
    navigate as a secondary action).
- The right-panel header carries a **breadcrumb** (`root › job › … › child › node`) so
  deep selections stay oriented.

### Live polling

- Forest summaries on the slow tier (~10s) — catches new subagents, forest-wide status
  changes, badge updates.
- The step tree of each **currently-expanded and live** job on the fast tier (~2s);
  collapsed jobs don't poll. Visibility-aware (existing `document.visibilityState`
  gate).

### Edge case — external-agent jobs

`claude` / `codex` jobs record no step/span tree (their internal loop is opaque). Such
a job node is a leaf; selecting it renders the persisted `session_messages` transcript
in the right panel (the fallback already in the page today).

## Data model — already supports this

No new domain types are needed; the backend already models the cross-session tree:

- `baybo_model::Session` (`crates/model/src/session.rs`): `root_session_id` (one query
  gets every descendant) and `lineage: Option<Lineage>`.
- `baybo_model::Lineage`: `{ parent_session_id, parent_job_id, parent_span_id,
  kind: Subagent }` — `parent_span_id` is the parent's `ToolCall(spawn_subagent)` span,
  the exact attach point, and disambiguates sibling subagents from one job.
- `baybo_query::QueryApi::lineage_tree(root_session_id)` (`crates/query/src/lib.rs:541`)
  — BFS over `list_lineage_children`, already cycle-capped at `MAX_DEPTH = 32`.
- `list_active_subagents` (`:511`) for the live set.

Trace wire types the front-end mirrors: `app/web/src/types/trace.ts`
(`StepKind::subagent { child_session_id }`, `SpanKind::subagent_stub { child_session_id }`).

## PR1 — front-end restructure (no backend change)

Delivers pains **1 + 2**. Single session, reusing the existing per-session endpoints
(`GET /v1/traces/{session_id}` overview + `/jobs/{job_id}` step tree). Subagent
spans/steps stay **leaves that link out** via today's `navigate('/traces/{child}')`.

- Rewrite the body of `TraceSessionPage.tsx` from 3-pane to 2-pane tree + detail.
- New tree components (`app/web/src/components/trace/…`): a recursive tree row + the
  Job / Step / Span node renderers, expand-state store, roll-up badge, filter bar.
- Expansion state: derived defaults (auto-expand failure path + selection ancestors) +
  ephemeral user toggles. Not persisted to the URL.
- On load, eagerly fetch the step trees of jobs whose `job_status_kind` is
  `failed`/`stuck` (to draw the failure path); other jobs stay lazy on expand.
- Right panel: reuse `SpanDetailPanel` + `JobSummaryPanel`; add the **Step panel** and
  the breadcrumb header.
- URL: keep `?job` / `?span` / `?tab`; add a `?step` selection.
- Update `app/web/src/api/mock.ts` fixtures to include a **failed** job/step/span so the
  failure-path auto-expand, roll-up badge, and step panel are exercised in mock mode.
- Verify: `pnpm --filter baybo-web build` (runs `tsc` + `gen:api`) green; no new clippy
  surface (front-end only).

## PR1.5 — information-density layer (from a field survey)

A survey of 8 agent-trace products (LangSmith, Langfuse, Arize Phoenix, Braintrust,
W&B Weave, Datadog LLM Observability, Vercel AI SDK, Pydantic Logfire) showed our tree
structure is right but our *rows carry too little*. The convergent moves: inline metrics
on every row, roll-ups on collapsed parents, a waterfall over the same spans, sibling-
relative heat tint, kind colour, and inline I/O previews. Langfuse is the closest
reference. Shipped the **P0** subset (pure front-end, data already on the wire):

- **Per-kind colour stripe** on the left edge of every Step/Span row (`traceFormat`
  `stripe` literal — kept literal so Tailwind's scanner generates the utility).
- **Inline latency bar** scaled to the slowest sibling ("the row is the waterfall"),
  coloured by outcome.
- **Rolled-up token badge** on Step rows (`sumLlmTokens` over the step's llm spans).
- **Heat toggle** — sibling-relative latency tint (red ≥75%, amber 50–75%).
- **I/O toggle** — a one-line input/output preview under each span (`spanPreview`, from
  data already on the row; no message-log hydration).

**bench-web alignment.** Retargeted the row aesthetic to `bench/bench-web`'s `FlowRail`
(the in-repo reference trace viewer): **everything expanded by default** (`resolveExpanded`
default `true`; `neededJobIds` loads every non-user-collapsed job — nothing is hidden),
**icon discs replaced by one-char kind glyphs** (`L`/`t`/`M`/`A`… coloured by kind, plus
the left stripe), and flatter rows. Kept our warm palette + 2-pane (rail + rich detail
panel) rather than swapping to bench-web's cool `#3b60e4` scheme so it doesn't clash with
the rest of the dashboard. Also ported from FlowRail: a full-width **trace-overview strip**
(`TraceOverviewBar`) above the panes — a clickable **sequence minimap** (one cell per
step/span across all loaded jobs, kind-coloured via a literal `cell` field, red for
failures, click→select + scroll), token totals, wall-clock, counts, and **per-tool** call
chips, and a **clickable legend** (collapsible) — selecting a legend entry
highlights that group across both the minimap and the tree by dimming everything
else, so "where did it fail" or "where are the tool calls" reads at a glance. Kind colours are one distinct hue per group —
**LLM = green, Tool = orange, Memory = blue, Subagent = gold, Aux = gray**, with a red
failure overlay — shared across the glyph, left stripe, and minimap cell (`TRACE_LEGEND`
is the key). Plus a left **job column** (`JobAnchors`) — a document-outline
list that selects + scrolls the tree to a job (`data-job-id`), previewing the **user input**
that started each job (so jobs are told apart by what was asked, not by number) along with
its tokens, duration, and a red dot when it holds a failure. Hidden for single-job traces. Still not ported: per-step **ordinals** and
FlowRail's flat chronological model.

Roadmap (deferred): **P1** = a Timeline/waterfall *view* toggle (bars on a shared time
axis, width switchable duration/tokens/cost) + a flat sortable "spans table" mode + row
virtualization + sticky Step headers. **P2** = inline **cost** badges (cost is integer
micro-USD in the cost store, NOT on the trace wire — needs a join into the trace +
`/lineage` endpoints) + Datadog-style "focus on subtree" (pairs with PR2) + large-trace
summarization (render only errors + long spans → relieves the trace-size bloat).

## PR2 — subagent lineage nesting

Delivers pain **3**.

- **Backend:** new `GET /v1/traces/{session_id}/lineage` in
  `crates/gateway/src/api/admin/traces.rs`, reusing `QueryApi::lineage_tree`. Returns
  the lineage tree with each descendant session's job summaries (status/tokens — the
  same shape as `TraceOverview.jobs`, minus the step tree). Response body is **untyped
  JSON** hand-mirrored in `trace.ts`, consistent with the rest of the traces family
  ("untyped JSON on the Rust side by design"); no `openapi` / ts-rs churn.
- **Front-end:** one up-front `/lineage` call builds the whole nav tree + cross-boundary
  roll-up badges; per-job step trees + per-child `session_messages` stay lazy on expand.
  Child sessions nest in place under their spawning span; the drill-out navigate becomes
  a secondary "open as full page" action.
- Extend polling to refresh `/lineage` on the slow tier (new subagents + forest status).
- Mock: add a subagent lineage fixture (parent job spawning a child session with its own
  failing job) so the nested view is exercised in mock mode.

## Deferred / cheap-approximation decisions (on the record)

- **Collapsed-job failure badge without a loaded trace** uses `job_status_kind` as a
  cheap approximation (a `failed`/`stuck` job → badge). Once the tree is loaded the
  roll-up counts failing leaves exactly, but still keeps the badge for a `stuck`/job-
  level failure whose spans are all `ok`. Acceptable for an admin/debug surface.
- **Filtering eager-loads every job's step tree** (only while a filter is active) so
  results are complete. A backend content-search endpoint is out of scope; for a session
  with an enormous trace this loads all job trees — acceptable for an explicit filter on
  a debug surface, revisit if it bites.
- **External-agent detection** is `terminal job + zero recorded steps` (no external-agent
  marker on the trace wire). A live 0-step job is *not* mislabeled (it may just not have
  flushed steps yet); a terminal 0-step job renders its transcript.
- **No feature flag** — this is an internal admin/debug page; PR1 rewrites in place on
  the same `/traces/:id` route. Old 3-pane code is removed, not parked behind a flag.
