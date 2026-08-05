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
   cost. The agent's full working transcript is collapsed behind a
   "view transcript" link (reuses the trace viewer).
9. **No in-UI diff or merge** (revised 2026-08-05; the original
   merge-button decision was cut — the rail is too cramped for a
   meaningful diff, and the button went with it). The detail rail shows
   only the **branch name with a copy action**. Review rides the run
   transcript plus the branch checked out locally; merging is done by
   the assignee on request (an ordinary comment) or by the user in a
   terminal. Dragging to Done marks completion **without** touching git;
   entering Done/Cancelled reclaims the worktree — skipped with a
   timeline note if it holds uncommitted changes; commit-less branches
   are deleted outright, the rest kept until a later GC.
10. **Sub-issues with stages, aligned with multica**: one parent level plus
    integer stage barriers — when every child in stage N is Done, the
    parent's assignee is woken to drive the next stage; the parent card
    shows a done/total progress ring.
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
description, workdir (optional), daily budget. Workdir left empty →
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
  working/idle dots, click → agent profile), **Chat with lead** button
  (slides out an in-board chat panel on the right — the same layer as
  the activity drawer and agent profile, mutually exclusive; it never
  leaves the kanban page. The panel holds **multiple conversations**: a
  new-conversation action and a history picker in its header (auto
  titles + last-active times); opening defaults to the most recently
  active one, and the first conversation is created lazily on the first
  message. Each is a normal session for billing/context/trace, but
  appears **nowhere in the global chat list** — no project session does;
  kanban and chat stay separate worlds. History lives in the panel,
  paging upward like the chat page (no in-panel search in v1; the trace
  page remains the audit view, and durable decisions live on the issues
  themselves). The panel reuses the chat thread
  renderer (bubbles, collapsed work blocks) with a trimmed composer:
  attachments and mid-run interjection stay, model switch and slash
  commands don't — the lead's llm pin governs its model. The lead's
  IssueCreate/IssueUpdate calls render as inline event cards, and the
  board beside the panel grows the same cards in the same frame.
  Conversation is user-initiated in v1), **Activity** toggle
  (right drawer: the feed of status changes, run results, blockers, hires,
  budget events), settings (team management, budget knobs, archive).
- Clicking any avatar (team strip, card face, timeline, execution log)
  slides out the **agent profile panel** (same layer as the activity
  drawer, mutually exclusive; no dedicated route in v1): display name
  from `IDENTITY.md` beside the immutable `@handle`, birth audit line
  (`created_by` — user-created or hired by the lead), live run state
  shared with the board's status frames, assigned issues, recent runs
  with transcript links, the **llm pin editor** (`profile.llm`; empty
  follows `default-llm`; pool-only choices), persona file editors
  (SOUL/IDENTITY/memory — audited commits, same pipeline as agent
  self-edits), and the user-only **Remove from project** tombstone
  action. Only the lead's panel carries a chat button; other agents are
  reached by @mention in issue comments (no DM in v1).
- The team strip ends in a dashed **＋ (new agent)**: a small form —
  display name, immutable `@handle` derived from it, a one-line role
  description that seeds `SOUL.md`, optional framework
  (native/claude/codex) and llm pin. Unlike `ProjectAgentCreate` (which
  deliberately exposes neither knob), the user form may set framework and
  llm. Creations stamp `created_by = User` and share the `max_agents`
  cap with lead hires; ⚙ team management carries the same action.
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
  reason ("drag", "comment", "stage barrier", "retry"), status, duration,
  cost, Cancel / Retry / **View transcript** (links into the existing
  trace viewer for the run's session); token totals.
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
  nobody is doing). Guards: assignee alive, budget headroom, no pending run
  for the same issue (dedupe), self-loop suppression (an agent flipping
  its own issue's status inside a run does not re-trigger itself).
- **Run delivery uses the ledger discipline** (record-before-deliver,
  stamp-on-resolution, boot re-drive, idempotent replay — the cron
  delivery pattern). A run row is the ledger entry; a crash between
  enqueue and wake re-drives on boot.
- **Each issue owns a dedicated session**, created on first run, bound to
  project + issue, so a follow-up run sees what the last one did. This
  costs a waiter subtlety: cron mints a fresh session per fire, so its
  reconcile can take the first terminal turn it finds; a session hosting
  many runs would hand run #3 run #1's outcome. What makes it safe is the
  dedupe guard — at most one run per issue is ever in flight — so the
  newest terminal turn at or after the run's own enqueue is unambiguously
  the one being waited on. **No project session appears in the global chat list**
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
  observation, not an up-front classification. At reclamation a
  commit-less branch is deleted outright. `ToolContext`
  gains `project_id`; bash/edit/write for an issue run are rooted in the
  worktree. Persona/memory write tiers carry over verbatim from the
  multi-project spec (own persona + project-shared `PROJECT.md`/memory +
  `personas/USER.md`; audited commits on the per-project git lock).
- **Known gaps in the shipped worktree layer** (adversarial review,
  2026-08-05 — the escape it found is fixed; these are not):
  - ~~A run cannot commit~~ **fixed**: `<workspace>/work/.gitconfig` is
    written on first run and is `$HOME/.gitconfig` inside the sandbox, so
    git has a committer. Still **not per-agent** — attribution wants
    `GIT_AUTHOR_*` in the child env, and the Bash tool's env channel is
    also the exact-match redaction list for injected secrets, so an agent
    id put through it would be scrubbed out of every command's output.
    Splitting those two uses is the follow-up.
  - ~~The Bash description says the cwd is `work/`~~ **worked around**:
    it still does (`Tool::description()` takes only `&self` and is
    pre-rendered per process), but the issue brief now names the checkout
    and says commands run there, so the run is told the truth by the more
    specific and more recent text.
  - **`prepare_checkout` runs `git worktree add` inline on the router's
    `select!` loop**, which also serves every user message and agent
    response. A slow checkout is head-of-line blocking for the whole
    process.
  - ~~`work/projects/` collides with a project named "Projects"~~
    **fixed**: worktrees live under `work/.worktrees/`, and a slug cannot
    contain a dot.
  - **A retitle between runs strands the previous branch.** The branch is
    looked up by a name recomputed from the *current* title, so renaming
    an issue makes the next run cut a second branch and leave the first
    one's commits behind.
  - ~~`git worktree remove` needs care~~ **handled**: a dirty tree comes
    back as `Reclaimed::Kept` with git's reason for the timeline; the
    255-after-deleting case is detected by re-checking the directory and
    finished with a `prune`; a commit-less branch is deleted only after
    the tree is gone, because until then it is checked out in it.
- **No merge machinery**: the platform never merges. The assignee merges
  when asked (an ordinary comment-triggered run in its worktree) or the
  user merges in a terminal. Worktree reclamation runs when an issue
  enters Done or Cancelled — skipped with a timeline note if the
  worktree holds uncommitted changes; branches are kept until a later
  GC.
- **Stages**: on every child completion, a barrier check runs; when stage
  N empties, the parent assignee is woken through the same ledger.
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
   git gave lands on the timeline. A commit-less branch goes with its
   tree; one with commits is the deliverable and is kept.

   The merge-by-assignee flow needs no code: it is an ordinary comment on
   a live card, which now wakes the assignee in its own worktree.
4. **Team autonomy.** Lead bootstrap + triage loop, `ProjectAgentCreate`,
   budget gate, approvals on the timeline, stages + parent wake,
   activity feed drawer.
5. **Polish.** Cron→issue creation (autopilot-lite on the existing cron
   system), push/badge integration, board filters, priority-driven lead
   triage hints.

## Defaults chosen without a grill question (veto anytime)

- Priority field exists (`urgent|high|medium|low|none`) but only informs
  the lead's triage and the card face; no automatic ordering.
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
- Lead scheduling of Todo → In Progress is prompt-driven (the lead moves
  issues when concurrency frees up); no hidden auto-promotion machinery.
