# project — Kanban Boards, Issue Runs, and the Worktrees They Work In

`baybo-project` is the domain crate behind the kanban feature: a **project** is
a board plus a git repository plus a team of agents, and an **issue** is a card
on that board that agents can be put to work on. This crate owns every rule a
write has to satisfy, the ledger that turns "somebody dragged a card" into "an
agent is running", and the per-issue git checkout that run works in.

It owns no I/O it can avoid. `baybo-store` declares the persistence port
(`ProjectStore`, `AgentProfileStore`), `baybo-storage` implements it, the
gateway serves the REST/WS surface, and `baybo-agent` executes the runs this
crate records. What is here is the part that must have exactly one answer.

Related: [`storage.md`](storage.md) (the sqlite tables), [`agent.md`](agent.md)
(the run executor and the issue router), [`gateway.md`](gateway.md) (the admin
API and the board's push frames), [`agent-profiles.md`](agent-profiles.md)
(`TeamMembership` is a field on the profile row). The product-level rationale
and the phasing live in [`../todo/kanban.md`](../todo/kanban.md); where that
document and this one disagree about *behaviour*, this one describes the code.

## Problem

A board is a machine that starts agents. Five different surfaces can move a
card — a drag in the web UI, a REST call, an agent's own `IssueUpdate` tool, a
cron fire, an @mention in a comment — and each of them can be the thing that
puts an agent to work. If "does this start a run?" is answered in five places,
the board ends up with five answers, and the failure is not a wrong pixel: it
is two agents editing one checkout, or a card that says work is in flight while
nothing is running, or an agent working a card the operator explicitly stopped.

So the crate is organised around **chokepoints**. Each rule has one home, and
every door leads through it.

## Design

### Layout

| File | What it owns |
| --- | --- |
| `manager.rs` | `ProjectManager`: the whole write surface, and the enqueue chokepoint |
| `runs.rs` | The two run predicates (`triggers_run`, `accepts_runs`) and the ledger entry |
| `comments.rs` | `comment_delivery` — what a comment does besides being recorded |
| `mentions.rs` | `@handle` scanning, and when a mention is a handover |
| `stages.rs` | Sub-issues, `is_finished`, the stage barrier's two questions, the progress ring |
| `budget.rs` | `Headroom` and the UTC-day window a daily ceiling measures |
| `timeline.rs` | `diff_events` — an edit reduced to the entries worth writing |
| `worktree.rs` | The per-issue git worktree: create, branch, commit identity, reclaim |
| `approvals.rs` | `TimelineApprovalGate` — a run's approval prompts, on the card |
| `events.rs` | The `ProjectEvents` push port (the gateway implements it) |
| `tools/` | The six board tools an agent working a project can call |

Everything under `runs`/`comments`/`mentions`/`stages`/`budget`/`timeline` is a
**pure rule module**: no store, no clock beyond an argument, unit-tested in
place. That is what lets every caller *in this process* — the manager, the six
board tools, the REST routes — answer a question the same way without any of
them carrying a copy of the rule.

The web board is not one of those callers, and this is the seam to know about.
A composer has to say what sending will do while the text is still being typed,
so it cannot ask the server. There are three hand-written TypeScript mirrors in
`app/web/src/pages/projects/`: `commentHint` and `mentionHint`/`mentionQuery`
mirror `comments::comment_delivery` and `mentions::assigns_to`, and
`retryRejection` mirrors the card-level refusals `ProjectManager::retry_run`
answers with, so a button knows whether the click would be rejected before it is
sent. Nothing enforces the correspondence — not a generated binding, not a
shared schema — only the two test suites, one per language, asserting the same
cases, which for the refusals means asserting the **literal sentences**
(`the_retry_refusals_say_exactly_what_the_button_predicts`): the button quotes
them, so a reword on one side is a lie on the other. So widening `is_live_work`
by one column, or adding a run state that reads as idle, is a change on both
sides in the same commit; `cargo test` alone will be green with a board that
wakes an agent while the composer still promises "Records only".

One sentence over there is deliberately *not* a mirror. `HELD_RUN_NOTE` sits
beside a **working** button: a press on a budget-held card goes through
`enqueue`, which releases what the ceiling allows before it writes, so the press
is what starts the run. `retry_run` reads the hold before and after and reports
the start rather than the dedupe guard's conflict; only when the ceiling refuses
again does it answer, with the budget as the reason.

### The run ledger

A run is a row (`issue_runs`) written **before** anything can act on it, and
settled by whoever executed it. Record-before-deliver, the same discipline cron
delivery uses: a crash between recording and dispatch is a run the boot sweep
finds, not work that silently never happened.

```
                       ┌──────(restart)───────┐
                       ▼                      │
Held ──(released)──► Queued ──(claimed)──► Running ──(settled)──► Done / Failed / Cancelled
  │                    │                                              ▲
  └────────────────────┴─────(cancelled, or called off)───────────────┘
```

Nothing calls off a row that is *running*: the sweeps are the only callers and
both filter by status first — `release_holds` sees only `Held` rows,
`resume_project_runs` only `Queued` ones. A `Queued` row may still be one that
ran, because the process-start requeue rolls an interrupted `Running` row back
to `Queued` and leaves its session on it — the card carries `RunStarted` for it
and the transcript exists — which is why the call-off has two sentences and
picks between them per row rather than per caller.

`Held`, `Queued` and `Running` are the **unsettled** states, and a partial
unique index makes at most one of them exist per issue. That index is the
crate's single most load-bearing invariant: it is what stops two agents sharing
one worktree, and — with the waiter looking only at turns from its own row's
enqueue onwards — what lets a run treat the terminal turn it sees as
unambiguously its own.

### One enqueue path, three gates

`ProjectManager::enqueue` is the only function that writes a run row. A drag, a
REST move, an assignment, a comment wake, a retry, a stage barrier and an agent
tool all arrive there, and it asks three questions once each:

1. **Liveness** — `runs::accepts_runs`. A card the board has finished with —
   Done, or cancelled — takes no runs. Asked first, before anything is written.
2. **Dedupe** — the store's partial unique index. A refused write means the
   issue already has a run in flight; the caller sees `None`, which is the guard
   working, not a failure.
3. **Budget** — `budget::headroom`. The headroom is measured before the write
   (the hold release below needs it), but it never decides *whether* the row is
   written — only what happens to it afterwards. So an exhausted board records
   the work it owes as `Held` rather than dropping it.

Two predicates, not one, because they answer different questions:

- `runs::triggers_run` asks about an **edge**: did this particular write start
  work? It needs a `Transition` (before/after status and assignee), and it says
  yes on exactly two edges — a card *entering* In Progress with somebody on it,
  and an agent *put on* a card already sitting there, handover included.
- `runs::accepts_runs` asks about a **card**: does this issue take work at all?

The distinction is not academic, and getting it wrong is the bug this crate has
had twice. Only three of the doors into a run carry a transition — creating a
card, editing one, moving one. A comment wake, a retry and a stage barrier
arrive at `enqueue` with nothing but a row, and a released hold and a boot
re-drive never reach `enqueue` at all. A cancellation rule enforced on the edge
covers three doors and misses five — so it lives on the card, at the
chokepoint, and each sweep asks it again for itself.

The two **sweeps** that hand out rows recorded earlier (`release_holds` and
`resume_project_runs`) do not go through `enqueue`, so they ask its gates again
themselves, against the board and the card as they are *now*: a hold can outlive
the write that recorded it by a day, and the process can be down for a week.
`resume_project_runs` is where both are asked — **archived** once for the board,
through `writable_project`, and **liveness** once per row, through `live_card` —
and it is the entry point for both callers that hand a board its work back, the
boot sweep and a restore.

The two gates dispose of a row differently, and that difference is the rule.
A row on a **finished card** is **called off** — settled `Cancelled`, with a
timeline entry in the board's own name. Skipping it would leave it unsettled,
and an unsettled row holds the issue's dedupe slot: revive the card later and
every run on it would be refused with "this issue already has a run". A row on
an **archived board** is **left exactly where it is**. Archiving is a shelf, not
a judgement on the work — the card never stopped taking work, the board did —
and a restore hands the same row back out rather than making the operator notice
a called-off run and retry it. The slot it holds meanwhile blocks nothing,
because every door that could enqueue against it is refused for the same reason
the sweep is.

At a **process start** the work is split in two. `requeue_unsettled` rolls every
`running` row back to `Queued` in one statement and hands nothing back — an
orphan is an orphan whatever board it is on, an archived one included, and
rolling a row forward settles nothing. `resume_project_runs` then walks the
boards, each reading its own unfinished rows, so runs go out per board in issue
order, oldest card first, with no order promised across boards. Nothing
downstream depends on which run starts first. That sweep is spawned *alongside*
the server rather than before it (`gateway_cmd.rs`), so it races live traffic: a
restore landing in the instant before the requeue commits finds its orphans
still `Running`, skips them, and they wait for the next process start.

`retry_run` is the one caller that refuses a finished card itself rather than
letting `enqueue` do it silently. `enqueue` can only answer `None`, which
`retry_run` reads as the dedupe guard, so the operator would be told the card
already has a run when it has none and never will.

### One dispatch per row — which is not guaranteed

`claim_run` is scoped to
`queued`, so two dispatches of one row collapse into one *execution* — but that
is the claim, and the dispatcher does real work before it: it cuts the issue's
worktree (`worktree::ensure`, a check-then-`git worktree add` that is not
concurrency safe) and settles the row itself when that fails. Two dispatchers on
one row therefore race on the checkout, and the loser settles `Failed` — which
`settle_run` accepts, because it gates on `settled_at IS NULL` and not on
status, even though the winner has since claimed the row and is executing it.
The card would then show a git error while an agent is live, the winner's own
settle would be a no-op, and the freed dedupe slot would let a comment wake or a
retry put a second agent into the same worktree.

The callers narrow this; none of them removes it.

A **restore** is an edge — `set_project_archived`'s UPDATE is conditional on the
state changing, so its `true` is the archived→live edge rather than "the row
exists" — so it can only overlap a dispatcher that is mid-checkout right then.
A **process start** is not an edge at all. The sweep comes up alongside the
server (`gateway_cmd.rs` spawns it into the task tracker; the router is
assembled after), and `resume_project_runs` hands out every `Queued` row
`active_runs` returns with no filter on when the row was written. So a card
filed into In Progress in the first seconds of a process — by an operator, by
`issue_create`, by a cron-fired session — is dispatched by its own `enqueue` and
again by the sweep, and the first dispatcher is still inside `get_issue` →
`comments_for_brief` → `prepare_checkout`, which shells out to git. That window
is 100 ms to seconds on a real repo, not microseconds.

Two smaller shapes sit beside it: the restore one above, and the gap inside
`resume_project_runs` between `set_project_archived` committing and `active_runs`
reading — the board is writable across it, so a door into `enqueue` can write and
dispatch a row the sweep's read then hands out again.

Closing that means the row defending itself, and the cheap way is the failure
path settling only a run **nobody has claimed**: a second store method whose
`WHERE` carries `status = 'queued'` alongside `settled_at IS NULL`, called in
place of `settle_run` at that one site in the dispatcher. The predicate has to
be the status and not `session_id IS NULL` — a `queued` row keeps the session
of the run it was rolled forward from, so an ownership test spelled that way
would refuse to settle every resumed run whose checkout failed, and leave the
card sitting queued with nobody on it and its dedupe slot held. Reordering the
claim ahead of the checkout would work too, but the claim needs a session the
router mints, so it is a real reordering rather than a `WHERE` clause.

### Holds and release

An exhausted board records work instead of dropping it. The held row shows on
the card with figures (`BudgetExhausted`), and it starts the moment there is
headroom again.

Holds are released **by activity, not by a clock**. Four things reach
`release_holds`: a budget change, the boot sweep, a board coming back off the
shelf, and every enqueue that gets past the liveness gate onto a card with
somebody on it — which is every enqueue that was going to write anything. (An
enqueue the liveness gate refuses releases nothing; the board's other holds wait
for the next thing that happens on it.) A daily ceiling that rolls over while
nothing is happening therefore needs no timer — the first thing that happens
next releases the hold, and if nothing happens, nothing needed releasing.

The restore is the same idiom applied to the other read-only state: a board
nobody may write is a board where nothing happens, so its work waits, and the
act of making it writable again is the activity that releases it. That is why
`set_project_archived(false)` calls `resume_project_runs` — without it, a
shelved run would wait for the next process restart. On the *edge* only: a
board that was never away is not a board where nothing happened, and its queued
rows belong to whoever dispatched them.

Inside `enqueue`, the release happens *before* the write, and it cannot move.
On the board that actually needs releasing — the exhausted one — every card is
already holding its own dedupe slot, so every enqueue is refused and a release
placed afterwards would never be reached. The price is that a caller whose own
held run has just become affordable is told the issue already has a run in
flight, which is true, and it is the run they asked for.

### Stages and the barrier

Sub-issues are one level deep, enforced in both directions (a child cannot gain
children; a card with children cannot become a child). A child carries a
`stage` number, and `check_stage_barrier` asks **two** questions on the
transition into a finished state:

- **Announce** — `stages::stage_complete`: are this stage's own children all
  done? That is a fact about the stage, true whenever it happens, and it is all
  `StageCompleted` claims.
- **Wake** — `stages::barrier_opens`: that, *and* nothing earlier is still open.

They are separate because stages are planned up front, so a later stage
routinely empties while the board is still on an earlier one. Folding them
together loses one or the other: either the operator is told a stage opened when
nobody was woken, or a stage that emptied out of order is never mentioned at
all. And a parent holds one run at a time, so waking it on a stage the board has
not reached spends the slot the real barrier needs.

`stages::is_finished` — Done **or** cancelled — is the single definition of
"the board is done with this card", read by the barrier, the worktree
reclamation and the enqueue gate alike. A cancelled step counts out of its
stage: "cancel the step you are not doing" is how an operator unblocks a
barrier. It also leaves both sides of the progress ring, so a parent whose last
two steps were called off reads `3/3` rather than `3/5`.

### Worktrees, and the branch as the deliverable

Every issue that runs gets its own git worktree of the project's repository at
`work/.worktrees/<project>/<number>`, on a branch `issue/<number>-<slug>`. Two
cards worked at the same time cannot see each other's edits.

Three details that are easy to get wrong and are pinned by tests:

- **The directory is keyed on the number, the branch keeps the slug.** A title
  is editable, so a slug-derived path would strand the worktree on a retitle.
  A branch name is read once by a human and never renamed.
- **A worktree's repository must be writable too.** A worktree's `.git` is a
  *file* pointing into `<repo>/.git/worktrees/<name>`; the index, refs and
  objects live in the main repository. A process that can write the tree but not
  the repo can run `git status` — which exits 0 — and then dies at `index.lock`
  on commit. `Checkout` therefore carries both paths, and both are bound into
  the sandbox.
- **Reclamation never destroys work.** On the transition into Done or cancelled
  the tree is handed back. `git worktree remove` refuses a tree with modified or
  untracked files, and that refusal is passed through as `Reclaimed::Kept` with
  the reason on the card rather than forced. The branch goes with the tree only
  when it holds nothing the repository does not already have **and** `git branch
  --delete` agrees — two independent readings, because it is the one step that
  cannot be undone. `commits_ahead` returning `None` means git could not answer,
  which is never read as zero. A branch that still carries work is the
  deliverable: baybo does not merge, the operator decides.

`ensure_commit_identity` writes a fallback `.gitconfig` into the work dir, so a
sandboxed `git commit` has somebody to be. An issue run overrides it with its
assignee's identity through `GIT_AUTHOR_*`/`GIT_COMMITTER_*`, which beats every
config file git consults.

### Comments, mentions, and the deferred wake

`comments::comment_delivery` decides what a comment does besides landing on the
timeline, from the issue and its unsettled run:

| Situation | Delivery |
| --- | --- |
| Nobody assigned, cancelled, or parked in Backlog/Done | `RecordOnly` |
| Live work, nothing reading | `Wake` — start a run |
| A `Held` or `Queued` run exists | `WaitsForQueuedRun` — it assembles its brief later, so it will read this |
| A `Running` run exists | `AfterCurrentRun` — deferred |

The deferred case is the one with a moving part: the executor calls
`wake_on_comment` when the run settles and the issue's live-run slot is free
again, and that goes through `enqueue` like every other start. Writing the
ledger row directly would produce a run nothing ever dispatches, holding the
slot until the next boot.

An `@mention` on a card **nobody is on** is the commenter saying "you take
this", and it is applied through `update_issue` — the same path a drag takes,
so it gets the same trigger, the same timeline entry and the same refusals for
an agent that cannot run. In the *commenter's* name, not the operator's. A
mention on somebody else's card is a question, never a reassignment: treating it
otherwise would let a passing remark take work away from whoever is doing it.

### Sessions

An issue keeps **one session per agent that works it**, minted on that agent's
first run. Per agent rather than per issue because a session's `AgentBinding` is
write-once — it selects the persona and SOUL the turn runs as, the skills it may
reach, and the name its commits are authored with. A card handed from `@dev-1`
to `@dev-2` cannot take its session with it; the *run* moves instead, into a
session bound to the agent the board says is on it. Signing dev-1's name onto
dev-2's work is worse than an unattributed commit, because it names the wrong
somebody.

Session continuity is what makes a follow-up run see what the last one did.
Because at most one run per issue is in flight, the waiter can treat the newest
terminal turn at or after its own enqueue as unambiguously its own.

**Which** session a run is handed is decided in exactly one place, and it is not
here: `session_run_to_continue` in `baybo-agent`'s issue router picks the newest
run of the *same agent* that ever got as far as executing, and `issue_session`
mints a fresh session when there is none. A run's brief is a delta against that
same run (`session_run_before`), never against a second rule spelled the same
way: a brief bounded by a run whose session this one is *not* given would trim
the card's conversation as "already read" against a transcript that does not
contain it.

The brief keeps at most 16,000 bytes of comment text, newest first, and always
keeps the newest comment. If older comments are dropped it inserts an explicit
marker instead of presenting the tail as the whole discussion. A run taking
over after another agent also gets a warning that the shared issue worktree may
contain that agent's uncommitted changes.

`was_claimed()` is a ledger-level approximation: a process can die after claim
but before the first turn opens. In that narrow case the next brief may trim
against a transcript that never received the run. A sharper answer would require
joining the turn store; both session reuse and brief construction deliberately
use the same ledger predicate so they cannot disagree about the boundary.

"Ever got as far as executing" is itself one predicate with one home —
`IssueRunRow::was_claimed()` in `baybo-store`, on the row it is about, in the
crate `baybo-project` and `baybo-agent` both depend on (the router's `ever_ran`
is that function under the router's own name). It reads the **session**, not
`started_at`: the executor stamps both when it claims a run, but the boot sweep
clears `started_at` and leaves the session, so only the session still answers
for a run the process died in the middle of. This crate asks the same predicate
for a different reason — a run being called off after a restart is told it was
interrupted, not that it never started. A row that never claimed a session never
opened a transcript and never touched the checkout, so none of the three
questions — whose session this run continues, whose uncommitted changes are
waiting in the tree it is handed, what its call-off says — counts it at all.

No project session appears in the global chat list — issue runs and the lead's
planning session alike.

### The team

A project's team is `agent_profiles` rows carrying a `TeamMembership`
(`project_id` + `handle` as one field, never two nullable columns that could
disagree). Every board opens with a `@lead`, seeded **before** the project row so
a failure leaves an inert orphan rather than a visible board with no
coordinator.

Project-owned persona trees are grouped at
`personas/project/<agent_id>/`. New leads and teammates are minted with a
`project-<ULID>` id, which makes the grouped location deterministic anywhere
only the profile id is available. Existing project personas keep their older
unprefixed ids in the flat layout; they remain readable and are not moved
behind the operator's back.

- Handles are derived from the display name and then permanent, and unique only
  *within* a board — `@dev-1` here and `@dev-1` there are different agents.
- Removal is a **tombstone**: `deleted_at` is stamped and the membership row
  stays, because `issues.assignee`, `issue_runs.agent_id` and every timeline
  `actor` name an agent by id, and the board has to keep being able to say who
  did what. Handles stay reserved forever.
- An assignee must be a live teammate of *this* board on the `baybo` framework.
  A global chat persona has no handle here; a removed one cannot take new work;
  an external `claude`/`codex` profile cannot host a top-level session.

  The framework half is the one whose answer **expires**. A profile's framework
  is editable — `update_agent` pins it only for builtins — so an agent given a
  card as `baybo` can be on `codex` before its next run starts, and a row the
  boot sweep re-drives was recorded under whatever it was then. So
  `runs::can_host_a_session` is asked three times against one rule: by
  `validate_assignee` when the card is assigned, by `enqueue` before it records
  a row, and by the executor's `binding_for` before the answer is written into a
  session write-once. The executor's is not belt-and-braces — the sweeps hand
  out rows without passing through `enqueue`, so for those it is the only ask.
  It refuses rather than records: a top-level session bound to an external
  backend would still be run by the internal loop, so the card would name an
  agent that never worked it and sign that agent's name to the commits.
- An agent with a run in flight cannot be removed — the run reads its row, not
  the roster, so removing it would just hide who is doing what is happening.

**Resolving an id back to a handle is board-scoped**, and both producers of the
rows a renderer sees enforce it: `team()` is scoped in SQL, and
`agent_profiles()` filters on `team.project_id`. An id from another board
therefore falls out and renders as the id, which is what a reference this board
cannot name should look like. Because removal leaves the membership intact, a
*departed* teammate still resolves — that is the whole reason handles are
resolved server-side rather than against the live roster.

### Approvals

`TimelineApprovalGate` wraps the channel's type-level approval gate and writes
what it sees onto the issue's timeline — `ApprovalRequested` before the prompt
is answered, `ApprovalResolved` after, including on the gate's own
deny-on-timeout. Installed once over the channel, not per run, so there is
nothing to arm or disarm and nothing to leak when a run dies unusually.

Pending-ness is **derived**, never stored: a request with no matching resolution
on the same `call_id` is still open. A `pending` flag would be a second copy of a
fact the timeline already carries, and the two would eventually disagree.

Nothing in this crate derives it, though — the two readers each derive it from
what they can see. The board's attention badge counts the channel's **live
approval queue**, passed into `attention()` because this crate must not know
about channels and because the queue is the only thing that knows a prompt has
already timed out. A card's "answer this" list is derived from the **timeline**
in the client (`pendingApprovals` in
`app/web/src/pages/projects/timelineModel.ts`), which is where the call ids are;
it is a view rather than an authority, since the resolve endpoint answers from
the queue and can refuse a prompt the timeline still lists.

A prompt from an ordinary session passes straight through — the trigger lookup
says it belongs to no issue.

### Tools

Six tools an agent working a board can call: `IssueList`, `IssueGet`,
`IssueCreate`, `IssueUpdate`, `IssueComment`, `ProjectAgentCreate`. Hosted here
rather than in `baybo-tools`, like `cron`/`skills`/`subagent`/`task` — a crate
that owns a domain hosts its own `Tool` impls.

**None of them takes a `project_id`.** The board comes from the calling
session's `TriggerSource`, which is the entire security model: a tool that
accepted a board id would let one board's agent edit another's, and no
validation downstream can recover a scope the caller was allowed to choose. The
scope read fails closed.

Agents address cards as `#4` and each other as `@dev-1`, never by ULID — the
identifiers a person reads off the board, so an agent's comment and the
operator's refer to the same things by the same names. `IssueGet` narrates the
timeline as prose rather than tagged JSON, because its reader is a model
assembling context.

### Push

`ProjectEvents` is a port with four hooks — `project_changed`,
`board_changed`, `run_changed`, `timeline_changed` — so this crate never depends
on the gateway or the wire types. `timeline_changed` is separate from
`board_changed` because a comment moves no card, and a board watching one
signal would refetch every column to learn that somebody said something.

## Key Constraints

- **One enqueue path.** Nothing outside `ProjectManager::enqueue` writes an
  `issue_runs` row. Liveness, dedupe and budget are asked there, once each.
- **At most one unsettled run per issue**, enforced by a partial unique index,
  not by a check.
- **A run row is written before anything is told about it**, and a failed
  dispatch costs a delay until the next boot sweep, never a lost run.
- **A finished card takes no runs** — `stages::is_finished` is the one
  definition, and a row already recorded on such a card is called off rather
  than left unsettled.
- **Archived is read-only for a board's contents** — issues, comments, the
  team, a retry, and starting a run — enforced in `writable_project`, which
  every one of those writes starts with, the sweeps included (through
  `resume_project_runs`). Three writes do not ask, and each is deliberate:
  `set_project_archived`, or a board could never be restored; the operator's
  bookkeeping (`mark_read`, `cancel_run` — stopping work and noting it was seen
  are not additions to the board); and `record_event`, which describes work
  already under way rather than starting any.
- **Archiving is reversible, so it settles nothing.** A run recorded before a
  board was shelved is left unsettled — not called off, as a finished card's is
  — and the restore hands it back out, on the archived→live edge and nowhere
  else.
- **A queued row is executed once — not dispatched once.** The claim collapses
  two dispatches into one execution, but only after both have cut the worktree
  and either may have settled the row. The boot sweep hands out rows a live
  enqueue is dispatching right now, so this is a known open shape rather than an
  invariant; see "One dispatch per row" for the fix and where it belongs.
- **A timeline append never fails the thing it describes.** Losing the note that
  a card moved is bad; refusing the move because the note could not be written
  is worse.
- **A project's workdir may not overlap baybo's workspace**, in either
  direction, checked after canonicalisation because the sandbox resolves
  symlinks when it mounts. `work/` is the single exemption.
- **The board never merges.** A branch with commits outlives its card; what
  happens to it is the operator's decision, and asking an assignee to merge is
  an ordinary comment on a live card.
