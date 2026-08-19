# Kanban projects — a board that runs itself

Design spec, 2026-08-05 (rev 1). This is the productivity-shaped successor to
the Slack-room multi-project design (`docs/todo/multi-project.md` on the
unmerged `multi_project` branch). The verdict on that branch: the substrate
(projects, self-staffing agents, wake ledger, budget, workdir writes) is
right; the room-and-mention surface is not. This redesign starts **from
master**, keeps the substrate concepts, and replaces the interaction model
with a kanban board. The `multi_project` branch is a code quarry, not a base.

Reference product: [multica](https://github.com/multica-ai/multica) — the
open-source managed-agents platform. Where this spec says "as multica does",
the behaviour was verified against its source (clone inspected 2026-08-05).

## Vocabulary

- **issue** — the unit of work on the board. Table `issues`, type `Issue`,
  UI word "issue". Deliberately not *card* (`deck_cards` — the iOS widget
  dashboard — owns that word), not *task*/*job* (background jobs own those).
- **run** — one execution of an issue by its assignee agent. Table
  `issue_runs`. An issue accumulates runs; a run is retryable, cancellable,
  costed, and has a transcript.
- **board** — the five-column kanban of one project.
- **project** — the container: external repo workdir + private agent team +
  budget + one board. **lead** — the auto-created coordinator agent.
- The words *room* and *channel* do not appear anywhere in this feature.

## Decisions (grill session, 2026-08-05)

1. Redesign from master; `multi_project` is reference only.
2. An issue is a task with its own execution thread; **dragging is a
   command** — the board is the control surface, not a mirror.
3. Every issue executes in its **own git worktree + branch** of the project
   repo (shared `.git`, no mirror/clone — that decision stands).
4. **Project stays as the container**; one board per project. Team, memory,
   budget hang off the project as in the multi-project design.
5. **Full self-staffing is kept**: the lead triages unassigned Backlog
   issues (takes them or assigns teammates) and may hire new agents via
   tool, capped and audited. The user can always assign manually.
6. Discussion surfaces: per-issue **timeline** (comments + system events),
   a read-only per-project **activity feed**, and a **direct chat with the
   lead** for planning (the lead turns conclusions into issues via tools).
   No free-form rooms.
7. **Five columns** — `Backlog / Todo / In Progress / Review / Done`.
   Entering **In Progress** is the single execution trigger. *Blocked* and
   *Cancelled* are issue flags (badge / strikethrough), not columns.
8. Issue detail is a **multica-style two-pane route**: main pane =
   description + sub-issues + third-person timeline with a comment
   composer; right rail = properties, branch + diff, execution log, token
   cost. A run's working transcript opens **over the main pane** as a
   read-only conversation panel (revised 2026-08-18; the original decision
   was a link into the trace viewer), rendered by the chat page's own
   thread components. The trace viewer keeps the spans, per-call tokens
   and context window, and stays one press away from the panel's header.
9. **No in-UI diff or merge** (revised 2026-08-05; the original
   merge-button decision was cut — the rail is too cramped for a
   meaningful diff, and the button went with it). The detail rail shows
   only the **branch name with a copy action**. Review rides the run
   transcript plus the branch checked out locally; merging is done by
   the assignee on request (an ordinary comment) or by the user in a
   terminal. Dragging to Done marks completion **without** touching git;
   entering Done/Cancelled reclaims the worktree — skipped with a
   timeline note if it holds uncommitted changes; a branch is deleted
   only when **both** readings agree it holds nothing — ours, that it is
   exactly zero commits ahead of the main checkout (an uncountable branch
   is not an empty one), and git's, via `branch --delete` rather than
   `-D`, which refuses anything it considers unmerged. Everything else is
   kept until a later GC.
10. **Sub-issues with stages, aligned with multica**: one parent level plus
    integer stage barriers — when every child in stage N is Done, that
    stage's completion is recorded on the parent's card, and the parent's
    assignee is woken to drive the next stage **provided no earlier stage
    is still open**. Stages are planned up front, so a later one routinely
    empties first; that is worth saying and is not a barrier, because the
    parent holds one run at a time and an early wake would spend the slot
    the real barrier needs. The parent card shows a done/total progress
    ring.
11. The unit is named **issue**, accepting future ambiguity with forge
    issues in exchange for matching the reference product and GitHub-shaped
    intuition.
12. **No project list page** (2026-08-05, post-mockup review): the rail
    entry restores the last-visited project's board directly; switching
    projects lives in a top-left switcher pill on the board header.
13. **Create modal is document-first** (2026-08-05, owner screenshot):
    big free title + description, properties as a bottom pill-chip row
    (status pre-filled from the column `+`), footer with attach /
    "Switch to agent" quick-create / "Continue create" toggle / `⌘↵`
    submit. No boxed label-field form.
14. **No issue prefix** (2026-08-05): a single-operator product doesn't
    need `KAN-12`-style identifiers — issues render as plain `#N`
    (per-project sequence). No prefix column, no prefix field.
15. **Workdir is optional at creation** (2026-08-05): left empty, the
    project gets a fresh git-initialised `<workspace>/work/<name>/` —
    a project no longer requires a pre-existing external repo.
16. **All ten edge-semantics questions from the mockup review were
    approved as recommended** (2026-08-05) and are folded into the
    sections below: run survives drag-out of In Progress (Cancel is the
    only kill switch); assign-while-in-In-Progress triggers; Block/Cancel
    live in the properties `⋯` menu; unassigned-issue comments degrade to
    record-only; the create modal refuses In Progress without assignee;
    archived projects hide behind the switcher toggle with read-only
    boards; switcher dropdown stats are live; merging is
    comment-the-assignee or local (no platform merge); column counts
    exclude cancelled; comments on a queued run merge into its brief.
17. **The board drives Todo itself** (2026-08-09), reversing the original
    "no auto-promotion machinery" default. Todo is a queue the board
    empties: whenever fewer than `max_parallel_issue_runs` runs are in flight,
    it takes the top staffed card out of Todo, moves it into In Progress
    in its own name and starts it (`RunTrigger::Promoted`). It runs as a
    **periodic sweep** (one task per process, every few seconds), not off
    each board write: the write-driven version made seven call sites
    load-bearing, where forgetting one parks a board until restart. The
    cost is that a card starts up to one tick after it became ready. The knob is
    **per project, on by default** (`0` = the old manual board) and has no
    upper bound — how much a board may run at once is the operator's call,
    and the only refusal is a negative, at the wire edge where the sign
    still exists. Order is **priority first, then column position** —
    which is the order the lead's `IssueList` already reads a column in,
    and is *not* the web board's own top-to-bottom order. A card reaching
    Todo **unassigned** wakes the lead to staff it (`RunTrigger::Triage`,
    the one run whose agent is not the card's assignee), asked once per
    state of the card so a lead that leaves it alone is not asked again.
    The board never promotes a blocked card, never promotes a card that
    already has a run, never starts work it cannot afford, and never
    preempts — priority decides who gets the *next* free slot, not who
    keeps one. A manual drag is unaffected by the ceiling: it is a command.
18. **The board asks the lead; a patrol cron is not the mechanism**
    (2026-08-14). The same sweep that promotes also wakes the lead about
    the cards only it can answer for, one per pass, each with its own
    trigger and a brief that opens with *why*: a card sitting in **Review**
    with nothing running and a non-lead assignee (`RunTrigger::Review` —
    arrange the review), a card in **In Progress** with no run and nothing
    queued (`RunTrigger::Stalled` — work silently stopped), and the
    existing unstaffed-Todo wake (`RunTrigger::Triage`). Asked once per
    **card activity** — its `updated_at` or the settle of its newest work
    run, whichever is later; the lead's own coordination runs count on
    neither side — so a lead that leaves a card alone is not re-asked, and
    a reviewer's verdict settling *is* a new question; at most twice per
    unchanged card row, because the machinery's own comment-wake-settle
    echo would otherwise re-arm the question it answers. The lead's own
    cards take no question (no other party), and a card whose newest run
    was **cancelled** is not "stalled" — a stop is a decision, not a
    silence, and it stands until somebody acts on the card. This replaces the
    operator-authored hourly patrol cron (which read the whole board each
    tick, hand-checked these three shapes, and woke the lead by commenting
    on the lead's own card — an O(N×transcript) chat thread); a cron
    remains sensible only as a low-frequency fallback for what events
    cannot see. Blocked cards are asked about by nobody: a block **parks**
    a card's run everywhere the board acts on its own — the driver skips
    it, the boot sweep leaves its queued row where it lies, the budget
    release keeps its hold, the stage barrier wakes no blocked parent —
    and the unblock hands the run back out (through the same liveness gate
    as every other hand-back door). A parked row does not count against
    the board's parallel slots: nothing is executing under it, and a few
    long-blocked cards must not silence the whole board.

19. **A board has two daily ceilings, and money alone was never one**
    (2026-08-15). The per-project daily *spend* limit measures nothing on a
    subscription plan: `openai-subscription` is absent from the OpenRouter
    prefix table, so `pricing_for` misses and the factory ships
    `ModelPricing::default()` on purpose — subscription billing is
    account-level, not per-token — and every `cost_records.cost_usd` for
    such a board is `0`. `spend_since` therefore sums zero forever and a
    money ceiling **can never be reached however low it is set**. A board
    whose teammates pin an `openai-subscription` entry is an ordinary baybo
    board; the framework gate says nothing about which LLM entry an agent
    runs against, so this is the ordinary case rather than an exotic one.

    So `projects.daily_budget_tokens` sits beside `daily_budget_micros`:
    same UTC window, same rows, same `Held` → release-on-activity
    machinery, and the board stops when **either** is reached. Both are
    optional and both default to unset, because a board should work out of
    the box.

    Four things settled with it, each because the obvious alternative is
    wrong in a way that only shows up later:

    - **Tokens are `input_tokens + output_tokens`.** The cached and
      cache-creation columns are *subsets* of `input_tokens`, not additions
      to it — that is the stated convention `compute_cost_usd` depends on,
      and the Anthropic adapter folds its natively-disjoint buckets into it
      before the record is written. Summing all four charges a board twice
      for its cached prefix, and every existing cost fixture seeds
      `cached_input_tokens: 0`, so that bug would have shipped green.
    - **One store read, not two.** `spend_since` widened to return `Spend`
      rather than growing a parallel `tokens_since`, so there is one answer
      to "what has this board burned today" and the two ceilings cannot come
      to disagree about which rows are the board's. Restated because it is
      the same lesson as the `IN (SELECT …)`-not-a-join rule above.
    - **The hold names its ceiling**, through its own timeline variants
      (`TokenBudgetExhausted`/`TokenBudgetRestored`) rather than a "which
      unit" flag beside fields called `spent_micros` — a flag there would be
      a lie on the wire, and every past row would have to be re-read to know
      which unit it meant. When both ceilings are set, the one spoken in is
      **whichever has least room left** (exhaustion first, then the tighter
      fraction; ties to tokens), decided once in `speaks_money` and mirrored
      by the switcher's meter client-side. The first cut of this said "a set
      token ceiling always wins", which review caught: it stamped a
      permanent `TokenBudgetExhausted` row on boards held by *money*,
      claiming a ceiling was spent at 0% used. "Prefer the exhausted one"
      alone is not the fix either — a release happens when neither is
      exhausted, so it would report a card's hold and its release in
      different units.
    - **A hold is still just a hold.** No second `RunStatus`, no split in
      the attention count: one `release_held_runs` pass frees both, and the
      distinction lives only in the timeline entry and the wording around
      it. What did change is that no sentence outside that entry may say
      "over its daily budget" any more — an operator told that on a
      token-limited board goes and raises a dollar figure that was never the
      reason.

    Explicitly **not** in scope, and said here rather than left unsaid: the
    global `cost.spending_limits` gate has the identical defect and keeps
    it. This is the board's ceiling only.

20. **A sweep with no meter is a retry loop** (2026-08-15), from eleven
    days of runtime data. The boot sweep re-drove run
    `01M00C5EFKFDGDHJ05W076318T` across three restarts with no counter, no
    age check and nothing on the card — its 25-second resurrection re-sent
    ~347K input tokens before doing anything — and the DB held 98
    `run_started` entries for 94 runs, two wedged runs carrying three
    textually identical "started run #7" lines each. What follows from
    that:

    - **`issue_runs.resumes` is a column, not a count over events.** A
      timeline append is explicitly allowed to fail without failing the
      thing it describes, so a counter derived from `run_started` entries
      undercounts on exactly the boot where the append failed — and the
      loop comes back. It cannot ride `attempt`, which is the *issue's*
      run counter and means something else. It is bumped inside the
      requeue's own `UPDATE`, the one statement that observes an
      interruption.
    - **Two bounds, two shapes, because one meter cannot bound both.**
      `MAX_RUN_RESUMES` bounds a row that ran and was interrupted;
      `MAX_QUEUED_WAIT_HOURS` (from `created_at`) bounds a row nobody ever
      claimed. `started_at` is the *first* claim, so an age read off it
      would call off a run that executed an hour ago. The measured case
      for the second is a row that sat queued 66h and was then executed
      against a card blocked three days earlier.
    - **No backoff.** The sweep fires once per process start, not on a
      rate; a delay would only park the row and need a second thing to
      come back for it, which is the invisible loop the bound ends.
    - **The give-up settles `Failed`, not `Cancelled`.**
      `driver::newest_run_was_cancelled` reads a cancel as a stop that
      stands, so a cancel would keep the lead from ever being told.
    - **`runs::verdict` is the one home** for "may the board hand this
      recorded row out now?", asked by all four hand-out doors. Steady-state
      dispatch and the boot sweep disagreeing about `blocked` is what let a
      blocked card be executed at boot.
    - **A re-claim says `RunInterrupted`, not a second `RunStarted`.** The
      card is not given a run state for it: the row reads `queued` with a
      session on it, and a card that says "running" through a gateway
      outage cannot be fixed at all — nothing is serving that page.
    - **The blocked wake is the highest-value one**, and it is the wake
      decision 18 explicitly declined. A blocked card is invisible to the
      promoter, the hold release and the boot sweep, so it is the one card
      nothing ever comes back to; rglide #13 had an agent find the card's
      goal contradicted the language spec, refuse to code, and block with
      the question — and the DB has not one `unblocked` event in it. Scoped
      to **agent-authored** blocks, read off the timeline: an operator's own
      block is that operator saying stop, and adjudicating it would
      countermand them within a tick and bill a lead run for it.
    - **A board with no `@lead` is repaired, not warned at.** `lead_of` is
      a string comparison that answers `None` silently; the `baybo` board
      (created 2026-08-05, before the seed existed, its only teammate hired
      as `@leader`, handles permanent) has had **zero** coordination runs
      ever while rglide has 25. The driver seeds one on a miss. A board
      whose lead was *removed* keeps the handle reserved forever and can
      only be told so — once.
    - **The executor may say a run failed, not that it produced nothing.**
      Run `01KZXXP774BNEQKB1DWQAZBMGT` committed, commented and moved its
      card to Review 23s before a SIGINT settled it "stopped before
      producing anything", and the lead narrated an invented root cause
      into the permanent timeline. The card's own answer is the board's,
      and the real settle reason — provider text, or the `CancelReason`
      behind a system cancel — now reaches the card, because the lead reads
      the card and not the transcript.

    Deferred deliberately: renaming `issue_runs.attempt` to a
    run-sequence name. The column *is* a per-issue counter and the name is
    wrong, but the rename is cosmetic and churns four files nothing else
    needs to touch.

## Pages and interactions (`app/web`)

New rail destination **Projects** (`components/IconRail.tsx` `DESTINATIONS`).

### Entry and switching — no project list page

The rail destination opens **the last-visited project's board** directly:
the client remembers the last pid, falls back to the most recently updated
project, and with zero projects opens the create form. Deep links
`/projects/:pid` are unaffected. Switching is a **top-left switcher pill**
on the board header (`name ▾`); its dropdown lists projects with live
working count and today's burn, ends in a "New project…" action and a
"Show archived" toggle (archived projects are hidden by default; an
archived board is read-only until unarchived). Create form: name,
description, workdir (optional), daily budget, daily token budget, parallel
runs. Workdir left empty →
the server creates `<workspace>/work/<name>/`, git-initialises it, and
stores that path (an existing non-empty `work/<name>` is an inline error,
never silently reused); workdir filled → must be an absolute path to an
existing git repo.

### `/projects/:pid` — the board

- Five fixed columns with header counts (**counts exclude cancelled
  issues** — they measure live work); column set is not user-editable.
- **Card face**: priority icon, `#N` identifier, 2-line title,
  assignee avatar with a live "working" shimmer while a run is active
  (queued = dimmed), sub-issue progress ring `done/total`, blocked badge,
  branch chip once the branch has commits (never for research-shaped
  issues), updated-ago. Cancelled issues render
  struck-through and are filterable out.
- **Drag**: `@dnd-kit` multi-container (first such usage in `app/web`;
  `QueuePanel`/`SessionSidebar` are the existing single-list precedents).
  Cross-column drop = status change. Ordering is a **dense integer rank
  renumbered per move**, not a fractional position: the move request
  carries the destination column's full new order and the store rewrites
  it in one transaction, closing the source column's gap in the same
  statement run. This follows `SessionFolderStore::reorder`, which is the
  house precedent, and it has no drift to compact later. **Dropping into In Progress with an agent assignee enqueues a
  run** — no confirm dialog (multica's choice too); a toast reports
  "queued for @handle". Dropping an unassigned issue into In Progress
  bounces with a toast asking for an assignee. **Dragging a card out of
  In Progress never kills its run** — the shimmer follows the run, not
  the column; the execution log's Cancel is the only kill switch.
- Per-column `+` opens the create modal pre-filled with that column; the
  form refuses In Progress without an assignee (the modal twin of the
  drag bounce).
- Header: the project **switcher pill** (top-left), team strip (avatars +
  working/idle dots, click → agent profile), **Filter** menu (search,
  assignee, blocked-only, hide-cancelled; the trigger carries a count of
  active narrowings), **Activity** toggle
  (right drawer: the feed of status changes, run results, blockers, hires,
  budget events), settings (budget knobs, the board's markdown
  description, archive behind a confirmation).
- **There is no chat with the lead.** The board is the whole control
  surface: work is described on cards and agents are directed by
  @mention on a card, so a conversation running beside the board would be
  a second place where a board's intent lives, and the one place the
  timeline could not audit. The lead stays a *role* — it is hired with
  the board, coordinates, and cannot be removed.
- Clicking any avatar (team strip, card face, timeline, execution log)
  slides out the **agent profile panel** (same layer as the activity
  drawer, mutually exclusive; no dedicated route in v1): its name from
  `IDENTITY.md`, which **is** its `@handle` and is immutable — one word
  for one teammate, so nothing can drift against anything; the IDENTITY
  editor below refuses a write that moves the `Name:` line and so does
  the agent's own `Edit` — birth audit line
  (`created_by` — user-created or hired by the lead), live run state
  shared with the board's status frames, assigned issues, recent runs
  with transcript links, the **llm pin editor** (`profile.llm`; empty
  follows `default-llm`; pool-only choices), persona file editors
  (SOUL/IDENTITY/memory — audited commits, same pipeline as agent
  self-edits), and the user-only **Remove from project** tombstone
  action. Only the lead's panel carries a chat button; other agents are
  reached by @mention in issue comments (no DM in v1).
- The team strip ends in a dashed **＋ (new agent)**: a small form — the
  name, which is the `@handle` itself and the only moment it is chosen
  (handle grammar, refused in the form and again on the server), a
  one-line role
  description that seeds `SOUL.md`, optional framework
  (native/claude/codex) and llm pin. Unlike `ProjectAgentCreate` (which
  deliberately exposes neither knob), the user form may set framework and
  llm. Creations stamp `created_by = User` and share the `max_agents`
  cap with lead hires. The strip is the one place a team is managed;
  ⚙ carries the board's own knobs and nothing else.
- Live updates ride the existing owner-channel WS frame family (board and
  detail invalidate on issue/run/event deltas; no polling).

### `/projects/:pid/issues/:num` — issue detail

Two-pane route, no tabs (multica's shape):

- **Main pane**: editable title + description (markdown), sub-issue list
  grouped by stage with inline status/assignee edit, then the **timeline**:
  comments and system events merged in third person ("lead assigned
  @dev-1", "moved to In Progress", "run #3 failed", "hired @tester").
  Bottom composer with @mentions; a hint
  chip states what sending will do (see comment semantics below).
- **Right rail**: properties (status, priority, assignee, stage, parent,
  dates later) with a `⋯` overflow menu carrying the destructive
  low-frequency actions — **Block…** (with reason), **Unblock**,
  **Cancel issue** (the terminal negative; rows are never deleted);
  **branch** (name + copy — no diff viewer and no merge
  button, per decision 9); **execution log** — every run with trigger
  reason ("drag", "comment", "stage barrier", "retry", "promoted",
  "triage"), status, duration,
  cost, Cancel / Retry / **Read as a conversation** — the trace viewer's own icon
  that opens the run's transcript as a chat-style panel over the main
  pane (`RunTranscriptPanel`, fed by
  `GET /v1/projects/{id}/issues/{n}/runs/{attempt}/transcript`), never a
  navigation away from the card; token totals.

  Two consequences worth stating, because both are visible: the panel shows
  a **session**, and one agent's runs on a card share one — so an attempt's
  page also holds the attempts before it, each under its own run header.
  The transcript opens on the run's **brief** — the ask the rest of the page
  answers — shown **unframed**: `unframe_issue_brief` takes back off what
  `frame_issue_brief` put on (who the agent is, that nobody is waiting at a
  keyboard, where its checkout is), because that half is written for the model
  and rendering it whole buries the card's own line under six hundred
  characters of machinery. No per-reader knob: the only transcripts holding a
  brief are runs', so there is nothing for one to choose between.
- Approval requests raised by a run (tool approvals) render as timeline
  cards with approve/deny inline, and mirror into the activity feed.

### Comment semantics

A comment on an issue in Todo / In Progress / Review is **delivered to the
assignee**: if a run is **queued**, the comment simply lands in the brief
that run will assemble when it starts — merged, never a second run
(multica's coalescing for free); if idle it enqueues a run. In Backlog /
Done a comment just records.

**Mid-turn injection is not what shipped** (2026-08-05). A run executes on
a *one-shot* actor, and `build_oneshot_actor` deliberately does not
register the handle with the supervisor ("one-shot sessions have no
follow-up traffic"), so there is no mailbox anything can route to. Rather
than change actor lifecycle to get it, a comment on a **running** issue
records and a follow-up run is enqueued when the current one settles —
the comment is never lost and never interrupts, and the composer says so
("@dev-1 is mid-run — this is picked up when that run finishes"). Real
injection is a latency improvement, not a missing capability, and it is
the one thing that would want registering issue actors. On an **unassigned** issue the chip
degrades to record-only and suggests @mentioning a teammate — an @mention
assigns and then follows the assign-trigger rule below. The composer chip
announces which of these will happen before sending.

## Data model (sqlite, `crates/storage`)

- `projects` — id (ULID), name, description, workdir, budget knobs,
  `max_parallel_issue_runs` (how many runs the board starts by itself; `0` is the
  manual board),
  `archived_at`, timestamps. No hard delete.
- `issues` — id, project_id, `number` (per-project sequence), title,
  description, `status` (`backlog|todo|in_progress|review|done`),
  `priority`, `assignee_agent_id` (nullable), creator (`User |
  Agent(id)`), `parent_issue_id`, `stage`, `position` (fractional),
  `branch`, `blocked_reason` (nullable), `cancelled_at` (nullable),
  timestamps. No DELETE: cancel is the terminal negative. Issues carry
  conversation history and follow the session-data-is-core rule.
- `issue_events` — the timeline: id, issue_id, actor, kind (comment,
  status, assign, run lifecycle, hire, budget, approval), payload,
  created_at. The activity feed is a project-wide query over this table —
  derived, not stored twice (the cron-groups lesson).
- `issue_runs` — id, issue_id, agent_id, session_id, trigger, status
  (`queued|running|done|failed|cancelled`), attempt, timestamps, cost.
- Project agents reuse `agent_profiles` exactly as the multi-project
  design specified: nullable `project_id`, `handle`, `created_by`,
  `deleted_at`; global rosters filter `project_id IS NULL`.

## Execution pipeline (`crates/project`, new crate `baybo-project`)

- **Single trigger predicate**, server-side, one function (multica's
  `WillEnqueueRun` lesson): a transition into `in_progress` with an agent
  assignee enqueues a run — whether it came from a drag, a REST call, or
  an agent tool. **Assigning an agent to an issue already sitting in
  In Progress also triggers** (no pending run present — multica's
  `RunSourceAssign`; without this the board shows work in flight that
  nobody is doing), **including a handover to a different agent**.
  Guards: assignee alive, board liveness, budget headroom, no pending run
  for the same issue (dedupe), self-loop suppression (an agent flipping its
  own issue's status inside a run does not re-trigger itself).
- **The trigger predicate is not the only door**, and that is why the
  liveness guard does not live on it. A released hold, the boot re-drive, a
  retry and a stage barrier all reach a run with no transition to look at,
  so **"a card the board has finished with — Done, or cancelled — takes no
  runs" is asked once, of the card, on the single enqueue path**, and again
  by the two sweeps that hand out rows recorded earlier. A finished card
  has already had its worktree reclaimed, so a run there would cut a fresh
  checkout for abandoned work. A sweep that finds such a row **calls it
  off** (settles it `cancelled`) rather than skipping it, because an
  unsettled row holds the issue's dedupe slot and would refuse every run on
  the card if it were revived.
- **Run delivery uses the ledger discipline** (record-before-deliver,
  stamp-on-resolution, boot re-drive, idempotent replay — the cron
  delivery pattern). A run row is the ledger entry; a crash between
  enqueue and wake re-drives on boot.
- **An issue keeps one session per agent that works it**, minted on that
  agent's first run, so a follow-up sees what the last one did. Per agent
  rather than per issue because a session's `AgentBinding` is write-once —
  it selects the persona, the skills and the name commits are authored with
  — so a card handed from `@dev-1` to `@dev-2` cannot follow the card; the
  run has to move instead, into a session bound to the agent the board says
  is on it. This costs a waiter subtlety: cron mints a fresh session per
  fire, so its reconcile can take the first terminal turn it finds; a
  session hosting many runs would hand run #3 run #1's outcome. What makes
  it safe is the dedupe guard — at most one run per issue is ever in flight
  — so the newest terminal turn at or after the run's own enqueue is
  unambiguously the one being waited on. **No project session appears in the global chat list**
  — issue runs and the lead's planning session alike (the old
  filter-project-sessions todo lands here); the lead session is reached
  only through the in-board panel. The run brief = issue
  title/description + timeline delta since the last run + PROJECT.md +
  project memory, via the context crate's prompt assembly.
- Assignee is **any baybo-framework agent profile**. The claim that
  external claude/codex profiles "already execute as sessions" was wrong:
  `chat.rs` refuses to bind a top-level session to a non-baybo framework,
  and the external backend exists only inside the subagent spawner. An
  external assignee is refused at enqueue with that reason, and giving
  external agents a top-level leg is its own piece of work.
- **A run cannot reach the project's repository yet** (verified on master,
  2026-08-05). Three independent guards, each sufficient on its own, and
  all three have to be answered before isolation is even the question:
  1. **Bash's default cwd is somewhere else.** It is
     `ToolContext::workspace_root`, which the runtime sets to
     `<workspace>/work` — not the workspace root, and not the project
     (`crates/baybo/src/runtime.rs:681,750`). An unqualified `git status`
     in a run therefore executes in baybo's work dir, which is not a
     repository, and the model gets "not a git repository" for a project
     that plainly is one.
  2. **Bash's own jail forbids the spec's worktree location.**
     `require_within_work_dir` rejects any absolute cwd *or command
     argument* that is inside `<workspace>` but outside `<workspace>/work`
     (`crates/tools/src/builtin/bash/mod.rs:1332`). The location this doc
     originally named — `<workspace>/projects/<pid>/worktrees/…` — is
     exactly such a path, so it would be refused before the sandbox is
     ever consulted. Worktrees therefore live under
     `<workspace>/work/projects/<pid>/<number>-<slug>`, which is also
     where `materialise_workdir` already puts an auto-created project
     repo (`crates/project/src/manager.rs:249`).
  3. **A workdir outside `$HOME` is absent from the sandbox.**
     `SandboxAdapter::build_spec` hard-codes `writable_paths: Vec::new()`
     (`crates/agent/src/runtime/sandbox.rs:166`), so the field is dead in
     production under *either* policy — and the bwrap `Permissive` arm
     would not have honoured it anyway, since the `writable_paths` loop
     exists only in the `Workspace` arm (`crates/sandbox/src/args.rs:73`).
     The actual RW surface a run gets is `extra_root` (= `$HOME`) plus
     `<workspace>/work`, minus the denylist's masking tmpfs. So a project
     pointed at `~/code/foo` already works by accident, and one pointed
     at `/data/foo` is *absent* (ENOENT), not read-only. Docker binds
     `writable_paths` outside the policy match
     (`crates/sandbox/src/args.rs:306`), which is exactly how a hole like
     this stays hidden.

  The trap when fixing (3): all three existing `writable_paths` tests
  build their spec with a helper whose default policy is `Workspace`, so
  they exercise a branch production never takes. A new test must assert
  against `Permissive`, and must be shown to fail with the fix reverted.

  And the part that makes this urgent rather than merely broken: under
  the default `permission: auto`, a sandboxed command that fails is
  handed to a post-fail LLM judge, and a `sandbox_related + safe` verdict
  **re-runs the identical command on the host with the sandbox off and
  nobody asked** (`SandboxEscapeDecision::Run` →
  `run_unsandboxed_wrapped`, `crates/tools/src/builtin/bash/mod.rs`).
  An ENOENT on the project checkout is exactly the shape of failure that
  reads as sandbox-related. So the honest description of today's
  behaviour is not "the run cannot reach the repository" — it is "the
  run fails inside the sandbox and may then reach the repository with no
  sandbox at all." Binding the checkout properly is what stops the
  escape hatch from being the mechanism by which project work happens.

- **Worktree per issue**: created lazily at first run under
  `<workspace>/work/projects/<pid>/<number>-<slug>` (moved under `work/`
  for the reason in guard 2 above), branch
  `issue/<number>-<slug>` off the repo's default branch. **Worktree and
  branch are separate ideas**: the worktree is the execution sandbox and
  every run gets one regardless of task kind; the branch is the
  *deliverable* and only surfaces in the UI (card chip, rail box) once it
  has at least one commit ahead of base. A research issue never shows any
  branch element — its deliverable is its report on the timeline — and no
  issue-type field exists: whether code was produced is an after-the-fact
  observation, not an up-front classification. At reclamation a branch
  that adds nothing to the repository is deleted, and only when both our
  own count and `git branch --delete` say so. `ToolContext`
  gains `project_id`; bash/edit/write for an issue run are rooted in the
  worktree. Persona/memory write tiers carry over verbatim from the
  multi-project spec (own persona + project-shared `PROJECT.md`/memory +
  `personas/USER.md`; audited commits on the per-project git lock).
- **Known gaps in the shipped worktree layer** (adversarial review,
  2026-08-05 — the escape it found is fixed; these are not):
  - ~~A run cannot commit~~ **fixed**, and ~~not per-agent~~ **also
    fixed**: an issue run's commits say which agent wrote them. That
    needed the Bash tool's env channel split from its redaction list
    (`ChildEnv`) — they were one `Vec`, so anything injected was also
    scrubbed from output, and an agent's id appears in its own output
    constantly. It said so through `GIT_AUTHOR_*`/`GIT_COMMITTER_*`
    until that turned out to erase the operator's own identity, which
    beats every config file git consults; it now appends a
    `Co-authored-by:` trailer instead, and the identity itself is
    resolved on the host and handed over as an answer rather than as the
    operator's config file (the sandbox remaps `HOME`, so every `~`
    inside such a file re-expands against the workspace). The
    write-it-and-hope fallback that was supposed to cover this —
    `ensure_commit_identity`, at `<workspace>/work/.gitconfig` — had
    **zero production readers** and was deleted; the only thing that
    ever read it was a test helper built to model a sandbox shape that
    does not exist. See [`docs/modules/project.md`](../modules/project.md).
  - ~~The Bash description says the cwd is `work/`~~ **worked around**:
    it still does (`Tool::description()` takes only `&self` and is
    pre-rendered per process), but the issue brief now names the checkout
    and says commands run there, so the run is told the truth by the more
    specific and more recent text.
  - ~~`prepare_checkout` runs on the router's `select!` loop~~ **fixed**:
    the worktree is cut in the run dispatcher, which is already a spawned
    task, and the checkout path rides on `IssueRunEvent`. The router
    shells out to nothing.
  - ~~`work/projects/` collides with a project named "Projects"~~
    **fixed**: worktrees live under `work/.worktrees/`, and a slug cannot
    contain a dot.
  - ~~A retitle between runs strands the previous branch~~ **fixed**: an
    existing worktree is asked what branch it is on (`worktree::branch_of`)
    rather than having the name recomputed from the current title. The
    derived name is now only ever used to *create*.
  - ~~`git worktree remove` needs care~~ **handled**: a dirty tree comes
    back as `Reclaimed::Kept` with git's reason for the timeline; the
    255-after-deleting case is detected by re-checking the directory and
    finished with a `prune`; an empty branch is deleted only after the
    tree is gone, because until then it is checked out in it — and only
    when the commit count is `Some(0)` and `branch --delete` agrees, so
    neither "git could not answer" nor "git thinks it is unmerged" is
    read as "nothing was produced".
- **No merge machinery**: the platform never merges. The assignee merges
  when asked (an ordinary comment-triggered run in its worktree) or the
  user merges in a terminal. Worktree reclamation runs when an issue
  enters Done or Cancelled — skipped with a timeline note if the
  worktree holds uncommitted changes; branches are kept until a later
  GC.
- **Stages**: on every child completion, a barrier check runs. When stage
  N empties, that is recorded on the parent's card; the parent assignee is
  woken through the same ledger only if no stage before N is still open.
- **Lead + hiring**: project creation seeds the lead from a coordinator
  SOUL template. Issue tools for agents: `IssueCreate`, `IssueUpdate`
  (status/assign/stage — status moves flow through the trigger
  predicate), `IssueComment`, `IssueList`, `IssueGet`, plus
  `ProjectAgentCreate` (cap default 16, `INSERT … SELECT` guard, audited)
  — lifted conceptually intact from the multi-project design.
- **Budget**: per-project daily spend gate checked at enqueue; exhausted
  → runs stay queued with a budget event in the feed.

## Phasing (PR sequence, each a draft)

1. **Entities + board.** ✅ **Shipped** (`crates/project`, `/v1/projects/*`,
   `app/web/src/pages/projects/`). `projects`/`issues` tables, REST CRUD +
   move, per-project numbering, Projects rail entry, board page with dnd,
   detail-page skeleton. The board works as a manual tracker.

   Two fields the spec lists were deliberately **left out until the phase
   that reads them**, so nothing ships as a decorative column (the
   `workdir`-was-never-read lesson from the multi-project branch):
   `assignee_agent_id` waits for Phase 4's team, and the daily budget
   waits for Phase 4's gate. `parent_issue_id`/`stage` wait for Phase 4's
   barriers, and `branch` for Phase 2's worktrees. `blocked_reason` and
   `cancelled_at` are here now because a user can set them today.
2. **Execution.** `issue_runs` + trigger predicate + ledger, per-issue
   sessions, worktree/branch creation, timeline events + WS deltas,
   execution log + transcript links, comment delivery (wake/interject).
3. **Review plane.** ✅ **Shipped.** `issues.branch` (written only once
   the work has a commit, so "has a branch" and "produced something" are
   the same fact and cannot disagree), branch chip on the card face and a
   copy box in the rail, and worktree reclamation on Done/Cancelled with
   the dirty guard — uncommitted work is never destroyed, and the reason
   git gave lands on the timeline. A branch that adds nothing to the
   repository goes with its tree — but only when our count says exactly
   zero *and* `git branch --delete` agrees; one with commits, and one git
   cannot vouch for, is the deliverable and is kept.

   The merge-by-assignee flow needs no code: it is an ordinary comment on
   a live card, which now wakes the assignee in its own worktree.
4. **Team autonomy.** ✅ **Shipped.**

   - **Teams.** `agent_profiles` gains a `TeamMembership` (`project_id` +
     `handle` as one field, never two nullable columns that could
     disagree). `list()` filters `project_id IS NULL` in SQL so no caller
     can leak a teammate into the global roster. `@handle` is its own
     grammar — narrower than the id's in every direction, because it is
     typed from memory — and it is the agent's name, not derived from a
     separate one: `validate_agent_name` is `AgentHandle::parse`, on the
     one door both the operator's hire and `ProjectAgentCreate` come
     through. Permanent once given.
   - **Removal is a tombstone**, the third exception to this repo's
     hard-delete rule and for a reason the other two don't share:
     `issues.assignee`, `issue_runs.agent_id` and every timeline `actor`
     name an agent by id. There is no restore and no bin, and the handle
     stays reserved forever — reissuing `@dev-1` would silently repoint
     every entry that already said it. The roster is now the assignable
     set: a global persona and another board's teammate are both refused.
   - **Every project opens with a lead**, seeded before the project row so
     a failure leaves an inert orphan rather than a visible board with no
     coordinator. `PROJECT_LEAD_SOUL_TEMPLATE` carries no substitutions;
     the project's name reaches the agent through each run's brief.
   - **Six tools** (`IssueList`/`Get`/`Create`/`Update`/`Comment`,
     `ProjectAgentCreate`), hosted by `baybo-project`. **None takes a
     `project_id`** — the board comes from the session's
     `TriggerSource::Issue`, which is the whole security model, backed by a
     new `ToolTriggerScope::ProjectBoard` for visibility and a fail-closed
     check for calls that arrive anyway. Agents address `#4` and `@dev-1`,
     never a ULID. Cap 16, hires audited via `hired_by`.
   - **Budget gate at enqueue**, on the single path every trigger passes
     through. The row is written *before* the budget is consulted, so an
     exhausted board records the work it owes as `RunStatus::Held` rather
     than dropping it; holds are released by activity (any enqueue, a
     budget change, the boot sweep) rather than by a timer. `spend_since`
     uses `IN (SELECT session_id …)`, not a join — runs share sessions, so
     a join would count one call once per run that shared one.
   - **Stages + parent wake.** One level, enforced in both directions.
     Both the announcement and the barrier fire on the *transition* into a
     finished state — which is also what bounds a stage to one
     `StageCompleted` entry per completion — but they ask different
     questions: the entry asks only whether that stage's own children are
     all done, the wake additionally asks that nothing earlier is open. A
     cancelled step counts as finished and leaves both sides of the
     progress ring.
   - **Activity feed drawer**, derived from `issue_events` across the
     board. Paging tiebreaks on `id DESC` so same-microsecond entries
     cannot page unstably.

   - **Approvals on the card.** `TimelineApprovalGate` wraps the owner
     channel's **type-level** gate once, at boot, rather than being armed
     per run: nothing to disarm, so nothing leaks when a run dies oddly,
     and a board opened later is covered without registration. It asks the
     *session* what it belongs to, so an ordinary chat prompt passes
     straight through. Both halves are recorded — including the gate's own
     deny-on-timeout, because the prompt is exactly where a reader would go
     looking. Pending-ness is derived (a request with no resolution on the
     same `call_id`), never a stored flag, on both sides of the wire.
     Answering rides `POST …/issues/{n}/approvals/{call_id}`, which
     resolves the **card** before touching the queue: the queue is keyed by
     call id alone, so that check is the only thing stopping one board from
     answering another's prompt.
5. **Polish.** Three of four shipped; cron→issue is designed and not built.

   - **Board filters.** ✅ Free text over title and `#number`, an assignee
     picker with a separate "Unassigned", a blocked-only toggle, and the
     cancelled toggle, all in the query string so the board→detail→back
     loop keeps them. Client-only: the board already fetches every issue,
     and a client that only saw a filtered board could not describe a
     column to a move request.

     It landed on top of a **live corruption bug** it would otherwise have
     made worse. `board` state *was* the filtered board, and a move sends
     its destination column's full new order, which the store applies by
     renumbering exactly the numbers it is handed — so dragging in a column
     holding a hidden cancelled card left that card on a colliding rank,
     silently. Fixed on both sides: the store's contract is now checked
     (`validate_column_order`), and the client holds the whole board with
     the view derived. That in turn required `Drop` to name an anchor
     *card* rather than an index, because an index is only meaningful
     against the list it was resolved on.

   - **Attention badge.** ✅ `GET /v1/projects/attention`, and a plain **red
     dot** on the rail's Projects entry. Not a count: the entry opens exactly
     one board (decision 12), so any number there is one the click cannot
     discharge — it said "3" and stayed at "3" through everything the
     operator did on the board it actually opened. The countable form of the
     signal lives **on the cards**, where each number is one card away from
     being cleared; the switcher dropdown still carries each board's own
     total, and its trigger wears a dot of its own when a board *other* than
     the open one is lit, because that dropdown is the only thing that says
     which board.

     Four signals on two contracts. `approvals` and `held` clear when the
     operator does the thing they were going to do anyway; `unread` — an
     agent's comment, an agent moving a card into Review — clears by being
     read. The cursor is `issues.read_at`, **per card**, stamped when the card
     is opened. This supersedes the original decision to leave both read-state
     signals out rather than approximate them: the approximation that made it
     a bad trade was a board-level cursor, which clears the question asked on
     #7 because the operator opened #3.

     `failed` sits on **both** contracts, deliberately. The card is broken
     until somebody acts on it, and says so the whole time; the rail's mark is
     only a pointer, and a pointer that survives being followed is noise — the
     operator opens the card, reads the failure, and the mark is still lit over
     a card they may be deliberately leaving until tomorrow. So the rail reads
     `failed` through the same `read_at` cursor and the card does not, and a
     card that fails again relights the rail off that same cursor. The two
     disagree in one direction only: the rail can go quiet over a board that
     still shows the failure, never the reverse.

     A `failed` card wears the marker on its own face and has a **Run again**
     button on its detail page (the retry endpoint had shipped with nothing
     calling it), plus a `failed` narrowing in the board filter. The badge
     had been counting failures on a board where no card admitted to one,
     which is the shape every "the dot won't clear" report actually had.

     Three ways to work through what is new, added once the counts were on
     the cards and clearing them meant opening them one at a time. An
     **Unread only** narrowing in the filter menu; every column **hoists its
     unread cards to the top**, a reading order laid over `position` that
     never rewrites it and that is dropped for the length of a drag; and a
     **Mark read** button in the header, on `POST
     /v1/projects/{project_id}/read`, carrying the board's own total because
     the press is what empties it. That endpoint is *not* the board-level
     read cursor this design threw out: it stamps the same per-card
     `read_at`, one card at a time, and it fires because the operator asked
     rather than because a page loaded — which was the whole defect.

     **Push is deliberately not part of this**, and not because of one
     predicate. The iOS Projects tab is a placeholder, a push payload can
     only address a session, and the tap handler touches that session into
     the phone's chat list — which is exactly what the project-session
     exclusion exists to prevent. `APPROVAL_TIMEOUT` is also a gateway-wide
     300s, so pushing an approval deadline to a locked phone is theatre.
     Board push is its own phase. State plainly in any PR: a web rail badge
     reaches an operator with a tab open, not one away from their machine.

   - **Priority-driven triage hints.** ✅ Facts on the `IssueList` result,
     not behaviour: per-agent `working_on` (from runs, never from the In
     Progress column — a run outlives its column), `you` and `lead` on the
     roster, the board's held runs and remaining budget, and parent/stage/
     progress per row. All derived over the *whole* board, because the
     canonical triage call filters to unassigned cards and a load derived
     from that set always reads as an idle team. Nothing ranks or promotes:
     auto-ordering and auto-promotion are both forbidden above.

   - **Cron→issue creation.** ✅ A job can be pointed at a board
     (`project_id` on the job, riding the `data` blob), the execution
     snapshots it so the boot re-dispatch rebuilds a bound fire, and the
     fire's `TriggerSource::Cron` carries it. `project()` — not `issue()`,
     not `is_project_session()` — is what makes the board's tools visible
     and scoped, so a bound fire stays an ordinary cron conversation:
     listed, pushable, and still able to call `report_nothing`. The fire
     runs as the board's **lead**, or its cards would be signed with a raw
     ULID and written into the wrong memory partition.

     `issues.source_key` under a partial unique index
     (`WHERE source_key IS NOT NULL AND cancelled_at IS NULL AND status <>
     'done'`) is the structural answer to "a daily job opens 365 identical
     cards": one live card per key per board, and the key is released when
     the card is finished or cancelled, so next month's occurrence gets a
     fresh one. `IssueCreate` takes at most a *suffix* and the server
     namespaces it by job id — so a job can collide with neither another
     job nor anything a person opened, and **omitting it gives the safe
     behaviour**, which is what stops the naive reminder duplicating
     itself. `CronCreate` inherits its board from the calling session like
     `origin_session` does, so an issue run cannot schedule work onto a
     neighbouring board; only `POST /v1/cron` names one, validated in the
     handler rather than by growing `baybo-cron` a `ProjectStore`.

     Not done, deliberately: a board-naming line in the fire's prompt
     framing (the tools are board-scoped, so the fire discovers its board
     through `IssueList` and needs no framing to find it), and re-pointing
     a live job at a different board (its past fires filed cards on the old
     one, so its execution history would describe work on a board it no
     longer touches).

6. **Attachments.** ✅ **Shipped**, and wider than decision 13's one-line
   promise: the create modal's footer attach exists, and so does attaching
   to a card's **description** on the detail page and to any **comment**.
   Any file type, not images only — which forced the download affordance
   the dashboard had never had (chat renders a non-image as a dead chip
   with no way to get the bytes back, so "parity with chat" would have
   meant a PDF you could attach and never open).

   The design is in [`docs/modules/project.md`](../modules/project.md#attachments);
   the three decisions worth carrying here are that the server reads a
   file's type and size **off the blob store** rather than from the client
   (they are what the context budget spends), that nothing stores a
   `kind` because the mime already answers it, and that markdown-embedded
   refs were never viable — `react-markdown` blanks the URL scheme and an
   agent's wholesale `description` rewrite would orphan them anyway.

   An issue run is handed its card's files as real content blocks, capped,
   so an agent **looks at** a mockup instead of reading its filename; an
   agent hands files back through `IssueComment`. iOS is untouched, because
   there is no board there to touch.

## What is still not built

Everything in "Pages and interactions" above now exists. Three things
remain, all recorded with their reasons rather than left to be re-derived:

- **Mid-turn injection.** A comment on a card whose run is *executing* is
  picked up by a follow-up run when that one settles, never lost and never
  interrupting. Real injection would deliver it at a tool boundary inside
  the running turn. It is a **latency** difference, not a capability one,
  and the only thing that would want issue actors registered with the
  supervisor — `build_oneshot_actor` deliberately does not register them
  ("one-shot sessions have no follow-up traffic"), which is precisely the
  assumption injection would break. Worth it only for "stop, you are going
  the wrong way".
- **Push about a board.** Not one predicate away: the iOS Projects tab is a
  placeholder, a push payload can only address a session, and the tap
  handler touches that session into the phone's chat list — which is what
  the project-session exclusion exists to prevent. `APPROVAL_TIMEOUT` is
  also a gateway-wide 300s, so pushing an approval deadline to a locked
  phone is theatre. Its own phase. The rail badge reaches an operator with
  a tab open, not one away from their machine; say so rather than letting
  the badge imply coverage it does not have.
- **Re-pointing a cron job at a different board.** Refused on purpose: its
  past fires filed cards on the old board, so its execution history would
  describe work on a board it no longer touches.

## Defaults chosen without a grill question (veto anytime)

- Priority field exists (`urgent|high|medium|low|none`). It informs the
  lead's triage, the card face, and the order the board takes work out of
  Todo (decision 17). It never reorders a column on its own. Neither does
  anything else that writes: the unread hoist added later is a **rendered**
  order and touches no `position`.
- No confirm dialog on drag; toasts + the timeline are the audit trail.
- Board grouping v1 is status-only; list/table/gantt/swimlane views, label
  system, and custom properties are all out of v1.
- No forge integration (GitHub PRs etc.) — the workdir is a local repo;
  merging happens locally, by the assignee or the user (decision 9).
  Forge sync is a future feature and one reason the *issue* naming will
  eventually need care.
- Issue identifiers are plain `#N` (the per-project sequence) — no
  prefix (decision 14). Cross-project ambiguity is acceptable in a
  single-operator product; a prefix can return if forge sync ever needs
  branch/PR-title matching.
- Everything the board starts on its own is decision 17; the lead may
  still move any card at any time, and a manual drag is never gated on the
  run ceiling.
