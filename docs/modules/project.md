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
| `manager.rs` | `ProjectManager`: the whole write surface, the enqueue chokepoint, and the executor's port |
| `settle.rs` | The settle chokepoint: the ledger row, the invalidation, and the timeline entry, as one sequence |
| `runs.rs` | The two run predicates (`triggers_run`, `accepts_runs`), the ledger entry, which earlier run a run continues, and `RunOutcome` |
| `dispatch.rs` | Turning a recorded row into an `IssueRunEvent` the executor can run |
| `brief.rs` | The brief a run is handed: the card, what has been said on it since, and which of its files fit |
| `attachments.rs` | The one door a file gets onto a card through: blob ids in, stored attachments out |
| `comments.rs` | `comment_delivery` — what a comment does besides being recorded |
| `actors.rs` | What an agent-facing surface calls the somebody a timeline entry names |
| `mentions.rs` | `@handle` scanning, and when a mention is a handover |
| `stages.rs` | Sub-issues, `is_finished`, the stage barrier's two questions, the progress ring |
| `driver.rs` | Which Todo cards the board starts by itself, in what order, and which cards the lead is asked about (staffing, review, stalled work) |
| `budget.rs` | `Headroom` and the UTC-day window a daily ceiling measures |
| `timeline.rs` | `diff_events` — an edit reduced to the entries worth writing |
| `worktree.rs` | The per-issue git worktree: create, branch, resolve the commit identity, reclaim |
| `approvals.rs` | `TimelineApprovalGate` — a run's approval prompts, on the card |
| `events.rs` | The `ProjectEvents` push port (the gateway implements it) |
| `tools/` | The six board tools an agent working a project can call |

Everything under `runs`/`comments`/`mentions`/`stages`/`budget`/`timeline`/`driver`
is a **pure rule module**: no store, no clock beyond an argument, unit-tested in
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

**Settling is three writes, not one**, and `settle.rs` is where they happen
together: the ledger row, the run's own invalidation, and the `RunSettled`
entry saying how it ended. A caller doing only the first leaves a card claiming
something the ledger under it contradicts — either a card that shimmers forever
because nobody was told, or a timeline that skips a run's whole ending. Every
door goes through it: the executor's `finish_run`, the operator's `cancel_run`,
the sweep's `call_off`, and the dispatcher settling a checkout it could not cut.
The actor differs per door and is the caller's to supply; the sequence is not.

Nothing in `settle.rs` consults `writable_project`, deliberately. A run archived
out from under mid-flight still has to finish its own bookkeeping — refusing
that write would strand the card, which is the failure settling exists to
prevent.

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

`ProjectManager::enqueue_as` is the only function that writes a run row —
`enqueue` is the same door for the ordinary case, filling in the card's own
assignee as the runner. A drag, a
REST move, an assignment, a comment wake, a retry, a stage barrier, a promotion
by the driver, the lead's wakes and an agent tool all arrive there, and it asks
three questions once each:

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
card, editing one, moving one. A comment wake, a retry, a stage barrier, the
driver's promotions and the lead's wakes arrive at `enqueue` with nothing but a row, and a released
hold and a boot re-drive never reach `enqueue` at all. A cancellation rule enforced on the edge
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

A row on a **blocked card** gets the archive treatment, not the call-off: the
block is somebody's decision to pause the card, and the sweep re-driving the
run would override it on nobody's authority (`driver::board_may_start`, the
same predicate that keeps the driver from promoting one and `release_holds`
from releasing one). The unblock is the door that hands the row back out —
`redrive_after_unblock` dispatches a parked `Queued` row directly and offers a
`Held` one to the budget's own release pass rather than starting around it.

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

Since the dispatcher settles through `settle.rs` like everybody else, that
losing settle now *announces* and writes its timeline entry. That makes the race
more visible, not more likely: the row was already wrong, and a board that
renders the wrong row faithfully is easier to diagnose than one that renders a
stale right one.

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
again by the sweep, and the first dispatcher is still inside `dispatch::prepare`
— `get_issue` → `comments_for_brief` → `worktree::prepare_for_issue`, which
shells out to git. That window is 100 ms to seconds on a real repo, not
microseconds.

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

### The driver: what the board starts by itself

Todo means "ready, waiting for capacity", and `ProjectManager::drive` is the
thing that notices the capacity. When a board has fewer runs in flight than
`projects.max_parallel_issue_runs`, it takes cards off the top of Todo — most urgent
first, then column order — moves each into In Progress in the board's own name
(`IssueActor::System`) and enqueues it as `RunTrigger::Promoted`. The default
ceiling is `DEFAULT_MAX_PARALLEL_ISSUE_RUNS`; `0` is the manual board, where nothing
starts that nobody dragged.

There is deliberately **no upper bound** on that number — how much a board may
have going at once is the operator's call, and any cap this crate invented
would be a policy nobody asked for. The single refusal is a **negative**, and
it happens in the gateway's `parallel_issue_runs`, at the `i64 → usize`
conversion: that is the last point where the sign still exists, and a
saturating conversion would hand the driver a slot count that empties the
whole Todo column in one pass.

The driver is **level-triggered**: it reads the board as it stands and closes
the gap, so calling it twice on an unchanged board does nothing the second
time. Nothing tells it a card became ready — it looks.

That is what lets **one caller** exist. `ProjectManager::run_driver`, spawned
once per process from `gateway_cmd.rs` beside the boot sweep, ticks every
`DRIVE_INTERVAL` (5s) and drives every unarchived board; in production nothing
else calls `drive`. The tick is also the boot pass, because a
`tokio::time::interval` fires its first tick immediately.

**This was the seven-call-site version first, and that was worse.** Ending
each write that can change either side of the gap — `create_issue`,
`update_issue`, `move_issue`, `finish_run`, `cancel_run`, `update_project`,
`resume_project_runs` — in a `drive` makes all seven load-bearing: the model
is sound (board state only moves through writes), but a new write path that
does not know about the driver leaves a board parked until the process
restarts, and nothing catches it. Two of those tails were already verbatim
copies of each other. On a tick, a forgotten anything costs one interval.

The price paid for that is **latency**: a card staffed in Todo, or a slot
freed by a settling run, waits up to `DRIVE_INTERVAL` before anything happens.
That is the trade, and it is why the interval is seconds rather than the
minute a pure safety net would want.

This is *not* the shape `release_holds` uses — that one is edge-driven off
activity, and the difference is worth knowing: a hold only matters to the card
holding it, so the next thing that happens on that board releases it. A
promotion is about the board's spare capacity, which the card that would use
it has no way to observe.

`drive` takes a process-wide lock. Reading the load and acting on it is not
atomic against the store, and three runs settling together would otherwise each
see the other two's slots as free.

A pass **releases holds before it counts slots**, and that order is load-bearing
rather than tidy. A held run is not in `board_load.working`, so counting first
sees an idle board — and the first promotion's `enqueue` then releases every
hold on its way past the budget gate, so the board runs the promotions *and*
the holds. Releasing first turns them into `Queued` rows the count can see. The
case is not hypothetical: `update_project` releases what it un-parks itself, so
the situation where a tick is the first thing to see new headroom is the **UTC
day rolling over**, which nothing is notified about — exactly when a
budget-limited board is holding runs.

At a process start the driver is sequenced *after* `resume_unsettled_runs`, in
the same task (`gateway_cmd.rs`). They cannot be folded into one function —
`requeue_unsettled` rolls every `running` row back to `queued`, which on a live
board would orphan the runs actually executing, so it is once-per-process and
the sweep is not. But racing them in two tasks leaves a re-dispatch and a
promotion to be sorted out by `claim_run` alone, and there is no reason to
spend that guard here.

Five things it will not do, each of which is a rule and not a coincidence:

- **It will not promote a card whose assignee cannot host a run.** Asked in
  `promote`, *before* the move — and that ordering is the whole point. The two
  writes a promotion makes are a move and an enqueue, in that order and not
  atomic, so every reason the enqueue could refuse has to be settled first: a
  card moved into In Progress and then refused a run is stranded, because it is
  no longer in Todo for a later pass to find. `is_promotable` only knows that
  *somebody* is on the card; whether that somebody still runs on baybo changes
  under the board's feet, since an operator can move an agent to another
  framework long after it was assigned.
- **It will not promote a card that already has a run recorded.** A card holds
  one run slot; promoting one that is spoken for would move it into In Progress
  and then fail to start anything, which is the one state that column must never
  be in. This is not an edge case — a comment on a Todo card wakes its assignee
  where it stands, and a triage run sits on an unstaffed card by construction.
  Both are in the busy set, held runs included.
- **It will not overrule a block.** A person dragging a blocked card into In
  Progress is overriding the block deliberately; the board doing it would be
  overriding it on nobody's authority.
- **It will not start work while the budget is exhausted.** `enqueue` would
  record the run and hold it, leaving a card in In Progress with nothing running
  under it. The existing hold/release path already owes that work.
- **It will not preempt.** Priority decides who gets the *next* free slot, not
  who keeps one. A card already running keeps running when something more
  urgent arrives.

Ordering is `(priority, position, number)` — the same order `IssueList` already
reads a column in, deliberately, so "what is next in Todo" has one answer
whether an agent asks or the board acts. Note that the **web board sorts by
`position` alone**, so on a column with mixed priorities the card the board
takes next is not necessarily the one rendered at the top.

**Asking the lead.** Some cards are not work the board can start — they are
questions only the lead can answer, and the same pass that promotes asks them
(`ask_the_lead`), one card per pass because the lead reads the whole board when
it is woken. Three questions, in the order they matter, each its own trigger so
the execution log says which was asked (`RunTrigger::is_coordination`):

- **Review** — a card sitting in Review with nothing running on it and an
  assignee that is not the lead. Arranging the review is the lead's to do, and
  before this existed the handoff waited for a patrol cron: the review sat idle
  up to a full schedule interval.
- **Stalled** — a card sitting in In Progress with no run against it and
  nothing queued: work that has silently stopped. A blocked card is not
  stalled — the block is the explanation, recorded by somebody with the
  authority to pause the card. Neither is a card whose newest run was
  **cancelled** (`driver::newest_run_was_cancelled`): a cancel is a decision
  — a human's stop, or the board calling a row off — and waking the lead
  would countermand it within one tick. The stop stands until somebody acts
  on the card.
- **Triage** — a card that reached Todo with nobody on it: the board cannot
  start it, so the question is *who should do this*.

Cards whose assignee *is* the lead take no question at all
(`driver::takes_a_lead_question`) — those are the lead's own, its
communication thread included, and a question about them has no other party.

These are the coordination runs — the ones whose `agent_id` is not the card's
assignee — and the brief they are handed opens with *why* the lead was woken
(`brief.rs`'s coordination preambles), because the card itself does not say.

The spin this could obviously become is closed by `driver::already_asked`,
which compares each question's newest run against the card's **last
activity**: its `updated_at`, or the settle of its newest *work* run,
whichever is later. A lead that read the card and left it alone changed
nothing, so it is not asked again; editing the card, moving it, or a work run
settling on it (a reviewer's verdict, say) makes it a new question. Coordination
runs count on neither side — the lead looking at a card is not the card
changing. The guard is a comparison rather than a flag precisely so that "has
anything changed since the lead looked?" has no second copy that could
disagree.

Two refinements on that guard, both mechanical bounds the comparison alone
does not give:

- **The cap.** Work-run settles re-raise a question, but the coordination
  machinery *generates* settles — the lead's wake comments, the assignee
  answers, the settle re-arms the wake, two billed runs per cycle — so one
  question is asked at most `MAX_ASKS_PER_CARD_STATE` (2) times while the
  card row itself stands unchanged. Past the cap, only somebody editing,
  moving or restaffing the card asks it again.
- **A dead ask is not an ask.** A coordination run the dispatcher settled
  `Failed` before it was ever claimed never put a brief in front of the
  lead, so it does not satisfy the guard — the question stays open for the
  next pass. It still counts against the cap, so a checkout that refuses to
  cut is retried once, not every five seconds forever.

`finish_run` runs its settle-and-follow-up sequence **under the driver's
lock**. The gap between a run settling and its comment follow-up being
enqueued is otherwise a window a drive tick can win: the just-settled card
reads as stalled or as awaiting review, the lead's wake takes the card's one
run slot, and the follow-up the settling run owes is refused — a swallowed
nudge, with a billed lead run in its place.

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

**A run's commits stay the operator's.** An issue run is credited in the
message, not in the authorship: `BashTool` appends a
`Co-authored-by: <handle> <persona-ULID@baybo.local>` trailer and leaves
`user.name`/`user.email` to whatever git would have resolved anyway. It used to
inject `GIT_AUTHOR_*`/`GIT_COMMITTER_*` instead, which beats every config file
git consults — so an operator's own name disappeared from work they are still
accountable for.

Two details that pin it:

- **Reaching the trailer.** It rides a `git()` shell function in the same
  `sh -c` body as the uv shims, so it covers a direct `git commit …` and nothing
  else — `bash -c '…'`, a script, and `/usr/bin/git` commit without it. Those
  commits are still authored correctly; they just carry no attribution. The
  trailer is spliced *after* the `commit` word, because `git commit -m x -- path`
  ends in a pathspec list where a trailing `--trailer` reads as a file name.
- **Having an identity at all.** `ensure_identity_config` asks git, **on the
  host**, who this checkout commits as, and writes the answer to a file beside
  the worktree that the shell is pointed at with `GIT_CONFIG_GLOBAL`. Handing
  the operator's own `~/.gitconfig` across instead looks equivalent and is not:
  the sandbox remaps `HOME`, so every `~` in that file — an
  `includeIf "gitdir:~/work/**"`, an `[include] path` — re-expands against the
  workspace and silently resolves to nothing, exactly defeating the per-repo
  identity it was written to select, while `core.hooksPath`, pagers and
  credential helpers come along uninvited. Resolving here gets `includeIf`,
  XDG, system and repo-local evaluated as the operator sees them and carries
  two strings. It is written at **global** scope, so `.git/config` and anything
  the run sets mid-flight still outrank it; a key set nowhere falls back to
  `baybo <baybo@localhost>`, because a run that cannot commit is worse than one
  signed by an obvious placeholder. Nothing is cached — one `git config` per
  call, against a sandbox spawn that costs several times more — so editing your
  identity is obeyed by the next command.

### Comments, mentions, and the deferred wake

`comments::comment_delivery` decides what a comment does besides landing on the
timeline, from the issue and its unsettled run:

| Situation | Delivery |
| --- | --- |
| Nobody assigned, cancelled, or parked in Backlog/Done | `RecordOnly` |
| Live work, nothing reading | `Wake` — start a run |
| A `Held` or `Queued` run exists | `WaitsForQueuedRun` — it assembles its brief later, so it will read this |
| A `Running` run exists | `AfterCurrentRun` — deferred |

The deferred case is the one with a moving part, and it is the board's to
resolve, not the executor's: `finish_run` asks `wake_after_run` once the run
settles and the issue's live-run slot is free again, and that goes through
`enqueue` like every other start. Writing the ledger row directly would produce
a run nothing ever dispatches, holding the slot until the next boot.

Two things bound "somebody said something", and both are the reason the
predicate lives here rather than beside the executor, which can see neither:

- **The window is when the brief was read**, and that is *neither* instant the
  ledger row carries. `created_at` is the enqueue: a `Held` run can sit there a
  day before its brief is cut, and waking on all of it re-instructs the agent to
  redo what it just did. `started_at` is the claim, which lands *after* the
  brief — `dispatch::prepare` reads the card, then shells out to `git worktree
  add`, then the event crosses the run channel, then the router mints a session.
  That interval is the same "100 ms to seconds" the double-dispatch section
  measures, and everything said inside it is in neither the brief nor a
  claim-bounded window, so nothing would ever come back for it — while
  `comment_delivery` has been answering `WaitsForQueuedRun`, which promises the
  operator the opposite. So the dispatcher stamps `IssueRunEvent::briefed_at`
  immediately *before* it reads, and the executor hands it back to `finish_run`.
  Stamped before rather than after so a comment racing the read is over-read
  rather than dropped.
- **A comment by whoever would run next is not somebody asking for more.** The
  follow-up this check decides runs as the card's **current assignee**, so
  that is the profile the filter protects from waking itself — an agent
  posting progress through `IssueComment` would otherwise wake itself, and
  then wake itself again on whatever the follow-up says. For an ordinary run
  the assignee *is* the run's agent; for a lead's coordination run they
  differ, deliberately: the lead commenting "please continue" on a stalled
  card is exactly somebody asking the assignee for more, and this filter is
  what turns the lead's settle into the assignee's wake.

  The key is a **profile**, because a timeline entry records only its actor.
  One agent holding two live cards can comment from one onto the other and
  have it skipped here. Narrowing that needs the authoring run recorded on
  the event body — a stored-shape change — so until then the filter errs
  towards a missed nudge rather than a run that answers its own note and
  wakes on the answer.

An `@mention` on a card **nobody is on** is the commenter saying "you take
this", and it is applied through `update_issue` — the same path a drag takes,
so it gets the same trigger, the same timeline entry and the same refusals for
an agent that cannot run. In the *commenter's* name, not the operator's. A
mention on somebody else's card is a question, never a reassignment: treating it
otherwise would let a passing remark take work away from whoever is doing it.

**A comment may be nothing but files.** "Here, look at this" under a screenshot
is a real thing to say, so the emptiness rule is *no text and no files* rather
than no text. Everything downstream is unchanged by it: a file-only comment
mentions nobody, so it reassigns nothing, and it wakes the assignee exactly as a
worded one does — being handed a screenshot is being asked to look at it.

### Attachments

A file hangs on a card's **description** (`issues.attachments`, a JSON column)
or on **one comment** (`IssueEventBody::Comment.attachments`, riding the
timeline body that was already JSON). Both hold `IssueAttachment` — a blob id,
its mime, its size, and the name it was uploaded under.

Three things are decided once, here, and the reasons are what keep them decided:

- **A caller names blobs, never attachments.** `attachments::resolve` is the
  only constructor: it `stat`s each id and takes the mime and the size *off the
  store*, keeping only the client's filename. Those two numbers are what the
  context budget spends when the file reaches a model, so a caller's word for
  them is an under-pricing hole. Blob ids reach the manager as a *parameter*
  rather than as something a caller fills in on `IssueUpdate`, for exactly this
  reason — and stated exactly, because the type does not enforce it:
  `IssueUpdate` still carries a `pub attachments` field, since the store has to
  be told what to write, and `update_issue` **overwrites** it from the ids it
  was handed. A caller that sets it is ignored rather than refused; the field's
  own doc says so. What holds is that nothing a caller writes there reaches the
  row. What does not is a compile error for trying.
- **There is no stored `kind`.** Image / audio / file falls out of the mime
  (`baybo_model::MediaKind::of_mime`), so no discriminator can disagree with the
  bytes it describes. The web derives the same split on its own side to choose
  between a thumbnail and a chip — two readings of one rule, one per side of the
  wire, neither with a second copy.
- **Markdown-embedded refs were never an option.** `react-markdown` blanks any
  `src` outside its scheme allowlist and an `<img>` cannot carry the bearer
  `GET /v1/blobs/{id}` requires, so an embedded ref renders as nothing, silently
  — and `IssueUpdate`'s `description` is a wholesale replace, so the first agent
  edit would orphan every reference in it. Files are their own field, drawn
  beside the prose.

What a **run** sees: `brief.rs` names every file on the card and in its comment
window in the prose, and carries up to `MAX_BRIEF_MEDIA` of them as real
`ContentBlock`s — so an image is *looked at* rather than read as a filename, and
a PDF arrives as a native document. The card's own files come first and in full
(they are the specification); the rest of the budget goes to the conversation
newest-first. A brief that could not carry them all says so, because a text
budget (`COMMENT_BUDGET`) cannot bound this: a picture is nearly free in bytes
and thousands of tokens in price.

The probes behind those blocks are `baybo_tools::blob_media`, shared with the
gateway's chat ingest — one home for which cap an image probe gets and when it
is worth taking, since a third copy is where those drift.

Uploads are stamped `project:<project_id>` (`baybo_store::project_uploader_identity`).
**Nothing reclaims them yet**, and that is not why they are stamped:
`uploader_identity` is written once and can never be filled in later, while a
board has no hard delete at all for a reclaimer to hang off. Stamping keeps the
door open; claiming it buys something today would be false.

### What the executor may do

`baybo-agent` executes runs; it does not write boards. It holds an
`Arc<ProjectManager>` and nothing under it — no `ProjectStore`, no
`ProjectEvents` — so the set of board writes it can perform is exactly the two
this crate offers it:

| | What it does | Why it is one call and not three |
| --- | --- | --- |
| `start_run(run, session)` | Claims the row, then says `RunStarted` on the card | The claim is what stops two agents on one card, so nothing may say a run started without having won it |
| `finish_run(run, checkout, briefed_at, outcome)` | Settles, surfaces the branch, then follows up on comments | The order is a rule: the branch is read before the follow-up because a follow-up enqueues another run against the same checkout, and the settle is first so a card whose branch cannot be read still stops shimmering |

`briefed_at` is handed straight back from `IssueRunEvent` — the executor carries
it rather than deriving it, because it is the only record of when the run's
brief was read and the ledger row's own two instants both fall on the wrong side
of it. See the follow-up window below.

`RunOutcome` is the whole of what the executor decides — it watched the turn, so
`status`, `error` and `stopped_by_a_human` are its answers — and what those cost
the card is this crate's. `stopped_by_a_human` is separate from `status` because
the ledger row cannot carry it and it changes what the board owes: somebody who
pressed stop is not asking for a follow-up.

**The branch is the one artefact a board hands over**, since it never merges, so
`record_branch` is written to survive the awkward order rather than assume a
tidy one. It reads the checkout's *own* branch, so a retitle mid-run cannot
rename a ref git already knows; and when the checkout is gone it falls back to
the name the tree was cut with, so a card finished *before* its run settled —
which reclaims the tree — still surfaces one. `commits_ahead` is then asked of
the repository rather than the checkout, because the tree may be gone while the
ref it left behind is not. `reclaim_if_finished` shares the same fallback:
whichever of the two runs first, both name the same branch.

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

**Which** session a run is handed is decided in exactly one place: `runs.rs`'s
`session_run_to_continue` picks the newest run of the *same agent* that ever got
as far as executing, and `baybo-agent`'s `issue_session` — which mints a fresh
session when there is none — is its only caller. A run's brief is a delta
against that same run (`session_run_before`, its sibling in the same module),
never against a second rule spelled the same way: a brief bounded by a run whose
session this one is *not* given would trim the card's conversation as "already
read" against a transcript that does not contain it. The two predicates live
here rather than beside the executor because the brief is assembled here too,
and a rule with two homes is a rule with two answers.

The brief keeps at most 16,000 bytes of comment text, newest first, and always
keeps the newest comment. If older comments are dropped it inserts an explicit
marker instead of presenting the tail as the whole discussion. A run taking
over after another agent also gets a warning that the shared issue worktree may
contain that agent's uncommitted changes.

**Every comment in it is attributed** — `- the operator: …`, `- @qa: …` — and a
comment's own newlines are indented so a line inside one cannot read as the next
speaker. Unattributed, a card's discussion arrives as one voice: the operator's
instruction, a teammate's question and the agent's own note from its last run
all carry the same weight, and an agent asked something cannot tell who is
waiting on the answer. `actors.rs` is the one home for that name — the same
`@handle` / `the operator` / `the board` that `IssueGet` renders a timeline
with, so the brief and the card an agent reads afterwards never name the same
somebody two ways. It deliberately has no `you`: `you` means *the reader*, and
the web board's reader is the person while a brief's is the agent, so a shared
spelling of it would be a shared word for two different somebodies. An agent
that has left the team is still named; an id belonging to another board renders
as itself rather than under a handle this board never issued.

`was_claimed()` is a ledger-level approximation: a process can die after claim
but before the first turn opens. In that narrow case the next brief may trim
against a transcript that never received the run. A sharper answer would require
joining the turn store; both session reuse and brief construction deliberately
use the same ledger predicate so they cannot disagree about the boundary.

"Ever got as far as executing" is itself one predicate with one home —
`IssueRunRow::was_claimed()` in `baybo-store`, on the row it is about, in the
crate `baybo-project` and `baybo-agent` both depend on (`runs::ever_ran` is that
function under this crate's own name). It reads the **session**, not
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

- An agent's **name is its handle** — `validate_agent_name` is
  `AgentHandle::parse`, enforced on `ProjectManager::hire`, which is the one
  door both the operator's form and the lead's `ProjectAgentCreate` come
  through. There is no second, prettier name to drift against. Handles are
  permanent, and unique only
  *within* a board — `@dev-1` here and `@dev-1` there are different agents.
  Permanence is the schema's, not this crate's: `idx_agent_profiles_handle`
  keeps it unique and reserved, and the `agent_profiles_team_is_insert_only`
  trigger aborts any `UPDATE` that would move a membership at all.
- **The name is permanent too**, which is what makes that survivable: a handle
  frozen against a name that drifts would leave the roster and every mention
  disagreeing about who somebody is. Hiring is the only moment either is
  chosen. The rule lives one layer down as
  `baybo_workspace::name::rejected_rename` (see
  [`agent-profiles.md`](agent-profiles.md)) because the name is a line in the
  agent's own `IDENTITY.md`, not a column here — so it has to hold at the
  operator's two endpoints *and* inside the agent's own `Edit`/`Write`, and it
  is keyed on the `project-` id prefix, the only signal a tool has.

  That prefix is also the rule's limit, and it is worth naming rather than
  implying: a project agent minted **before** the prefix existed keeps a flat
  id, and nothing at the tool doors can tell it from a global persona, so it
  can still rename itself. Asking the store instead would fix those two doors
  and leave the other two answering a different question — and a predicate
  with two homes is how the four doors stopped agreeing in the first place.
  Boards opened by this build have no such agent.
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

### Attention: four signals, two contracts

`attention()` answers one question per board — is anything waiting on the
operator — as four counts: `approvals`, `held`, `failed`, `unread`. They divide
on **what discharges them**, and the split is the whole design:

- `approvals` and `held` clear by **acting**. Answer the prompt, give the board
  budget. Looking at them changes nothing a query could see, which is exactly
  why they need no stored state.
- `unread` clears by **reading**.
- `failed` is **both**, and is the only signal read by two rules. The *card* is
  broken until somebody acts — retry, finish, cancel, block — and wears its
  badge that whole time; nothing retries by itself, so a badge a glance could
  clear would take the board's own record of what is broken with it. The
  *rail's* mark is only a pointer, and a pointer that survives being followed
  is noise: it stays lit after the operator has read the failure, with nothing
  left to do about it but act on a card they may be deliberately leaving until
  tomorrow. So the board's count drops a failure once that card has been
  opened, while the card goes on saying so. A card that fails *again* relights
  the rail off the same cursor — one rule, not two.

The cursor is `issues.read_at`, one per card, moved by `mark_issue_read` and by
nothing else. Per card and not per board: an operator who reads the question
asked on #3 has not read the one asked on #7, and the board-level stamp this
replaced could only clear both or neither — it was fired by the board page's
load effect, so it also swallowed everything written between that page's
fetches and the POST landing.

Three SQL predicates in `crates/storage/src/sqlite/project.rs` are the single
home of these rules:

- `UNREAD_EVENT_PREDICATE` — an agent's comment, or an agent moving the card
  into Review, newer than that card's `read_at`. The actor filter covers both
  arms: the operator's own words and their own tidying are not news to them.
- `FAILED_CARD_PREDICATE` — a live card whose newest run failed. Both
  `card_signals()` (the badge) and `attention()` read it.
- `UNSEEN_FAILURE_PREDICATE` — and that run settled after `read_at`.
  `attention()` alone adds it; the card's badge must not.

The two failure predicates read the same run by construction: the `newest_run!`
macro is where "newest" is spelled, so `status` and `settled_at` cannot come to
be read off different rows.

`card_signals()` reads per card and `attention()` per board, so on any live
board the `unread` count is the sum of its cards' and the `failed` count is the
number of cards wearing the marker **that the operator has not opened since it
broke**. The two therefore disagree by design, in one direction only: the rail
can go quiet while the board still shows a failure. The reverse — a rail dot
outliving a board on which every card reads zero — is the drift these constants
exist to prevent; written twice, they would eventually disagree, and the badge
would be pointing at nothing the operator could find.

`attention()` alone excludes archived boards — the whole board, not row by row.
`card_signals()` does not, because it is already scoped to the one board its
caller asked for, and a shelved board's cards should still say what happened on
them while the operator is reading it. So a shelved board's cards can carry
counts the rail deliberately does not: shelving is the operator saying nothing
here is waiting on them.

`ProjectManager::board_cards` is the one door for anything that draws a card
face: it hands over `BoardCards`, which is the rows **plus** the resolved
`CardSignals`. Callers never receive the runs and the recipe — "did this card's
newest run fail" is a rule with one home, and a caller holding the ingredients
would answer it a second time and differently.

A hold whose card stopped accepting runs is settled by `call_off_dead_holds`,
which `drive` calls **above** every gate under it. `release_holds` returns early
on an exhausted budget and `promotions` returns early on
`max_parallel_issue_runs == 0`, so both deliberate ways to stop a board used to
also stop the only sweep that could clear a hold on a card the operator had
already cancelled.

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
  bookkeeping (`mark_issue_read`, `cancel_run` — stopping work and noting it was seen
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
