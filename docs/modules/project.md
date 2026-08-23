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
| `artifacts.rs` | Which regenerable build output an idle checkout may give back, and the one verb that offers it |
| `runs.rs` | The two run predicates (`triggers_run`, `accepts_runs`), which row still holds a card's slot (`is_unsettled`), the ledger entry, which earlier run a run continues, and `RunOutcome` |
| `dispatch.rs` | Turning a recorded row into an `IssueRunEvent` the executor can run |
| `brief.rs` | The brief a run is handed: the card, what has been said on it since, and which of its files fit |
| `attachments.rs` | The one door a file gets onto a card through: blob ids in, stored attachments out |
| `comments.rs` | `comment_delivery` — what a comment does besides being recorded — and whether a window of entries holds somebody asking for more |
| `actors.rs` | What an agent-facing surface calls the somebody a timeline entry names |
| `mentions.rs` | `@handle` scanning, and when a mention is a handover |
| `stages.rs` | Sub-issues, `is_finished`, the stage barrier's two questions, the progress ring |
| `driver.rs` | Which Todo cards the board starts by itself, in what order, and which cards the lead is asked about (staffing, review, stalled work, blocks, the Backlog the board filed) |
| `budget.rs` | `Headroom` and the UTC-day window a daily ceiling measures |
| `timeline.rs` | `diff_events` — an edit reduced to the entries worth writing |
| `worktree.rs` | The per-issue git worktree: create, branch, resolve the commit identity, reclaim |
| `approvals.rs` | `TimelineApprovalGate` — a run's approval prompts, on the card |
| `events.rs` | The `ProjectEvents` push port (the gateway implements it) |
| `stopper.rs` | The `IssueRunStopper` port — interrupt the turn under a live run — and the `TurnLifecycle` adapter a real assembly hands it |
| `tools/` | The six board tools an agent working a project can call |

Everything under `runs`/`comments`/`mentions`/`stages`/`budget`/`timeline`/`driver`
is a **pure rule module**: no store, no clock beyond an argument, unit-tested in
place. That is what lets every caller *in this process* — the manager, the six
board tools, the REST routes — answer a question the same way without any of
them carrying a copy of the rule.

The web board is not one of those callers, and this is the seam to know about.
A composer has to say what sending will do while the text is still being typed,
so it cannot ask the server. There are two hand-written TypeScript mirrors in
`app/web/src/pages/projects/`: `commentHint` mirrors
`comments::comment_delivery`, the block gate inside it included, and
`mentionHint`/`mentionQuery` mirror `mentions::assigns_to` **and the refusal
`mention_assignment` wraps it in** — a mention on a blocked card is recorded
and staffs nobody, so the composer says that rather than promising a handover
that will not happen. Nothing enforces the correspondence — not a generated
binding, not a shared schema — only the two test suites, one per language,
asserting the same cases. So widening `is_live_work` by one column, or adding a
run state that reads as idle, is a change on both sides in the same commit;
`cargo test` alone will be green with a board that wakes an agent while the
composer still promises "Records only".

The retry button has **no** mirror, deliberately: nothing about it is typed
into, so it can send and render what came back. Its refusals are still
sentences an operator reads (`the_retry_refusals_say_exactly_what_the_button
_predicts` asserts them literally), and `IssueDetailPage` puts the server's own
words in the error banner rather than guessing at them.

One sentence over there is deliberately *not* a mirror. `HELD_RUN_NOTE` sits
beside a **working** button: a press on a budget-held card goes through
`enqueue`, which releases what the ceiling allows before it writes, so the press
is what starts the run. `retry_run` reads the hold before and after and reports
the start rather than the dedupe guard's conflict; only when the ceiling refuses
again does it answer, with the ceiling as the reason — naming *which*
ceiling, because an operator told a token-limited board is "over its daily
budget" goes and raises a dollar figure that was never what stopped it.

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

`cancel_run` is that door for a run with no live turn to interrupt, and the
split is on **status**, not on whether a session was ever opened: a `Running`
row carrying a session is interrupted instead, through the `IssueRunStopper`
port, and settled by the executor watching its turn, so a stop and a settle
never race to record how the run ended. Everything else settles here — a
`Queued` or `Held` row that never started, and equally a `Queued` row that ran
for an hour before the process died, because `requeue_unsettled` rolls a
crashed row back without clearing `session_id` (the same reason the dispatcher's
ownership test below has to read `status` and not `session_id IS NULL`). A
session on the row is not evidence a turn is alive under it. `cancel_run` does
both halves itself: a caller asks the board to stop a
run, and is never handed the `SessionId` to interrupt on its own, because "how
a run is stopped" spelled in the route and again in the crate is one home too
many — and only one of the two would learn about the money ceiling below.

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

Nothing calls off a row that is *running*, and that rule lives in
`runs::verdict` rather than at the call sites: it answers `Stands` for a
`Running` row first, before it so much as reads the card. Three of the four
doors do filter by status on the way in — `resume_project_runs` sees only
`Queued` rows, `release_holds` and `call_off_dead_holds` only `Held` ones — but
`redrive_after_unblock` hands over whatever is unsettled, so a rule spelled at
the callers is a rule the fourth door does not have: one `update_issue`
carrying both an unblock and a cancel would settle the executing row under its
own executor, and stamp it interrupted, which it was not.

A `Queued` row may still be one that ran, because the process-start requeue
rolls an interrupted `Running` row back to `Queued` and leaves its session on
it — the card carries `RunStarted` for it and the transcript exists — which is
why the call-off has two sentences and picks between them per row rather than
per caller.

`Held`, `Queued` and `Running` are the **unsettled** states, and a partial
unique index makes at most one of them exist per issue. That index is the
crate's single most load-bearing invariant: it is what stops two agents sharing
one worktree, and — with the waiter looking only at turns from its own row's
enqueue onwards — what lets a run treat the terminal turn it sees as
unambiguously its own.

**The hand-out question has one home.** `runs::verdict(run, card, now)` answers
"may the board hand this recorded row out now?", asked of the card and the row
**as they stand** — not as they stood when the row was written. Four doors ask
it, all through `ProjectManager::card_for`: the boot sweep, the hold release,
the dead-hold sweep, and the unblock. `enqueue` does not, because it is the
door that *writes* the row and asks its own three gates. The verdict is one of
four: the row **stands**, it is **parked** where it lies, it is **called off**
because the card stopped taking work, or the board **gives up** on it. It says
nothing about the runner's framework — that answer expires too, and
`can_host_a_session` is asked of the store by the executor's `binding_for`,
which stays the only ask a swept row gets.

**Two bounds, because a sweep with no meter is a retry loop.** `issue_runs`
carries a `resumes` counter, bumped inside the requeue's own `UPDATE` — the one
statement that observes an interruption, once per process start per row.

- A row that ever ran is handed back out at most `MAX_RUN_RESUMES` times; past
  that it is settled `Failed`. There is no backoff, deliberately: the sweep
  fires once per process start rather than on a rate, so a delay would only
  park the row and need a second thing to come back for it — which is the
  invisible loop the bound exists to end.
- A row **nobody ever claimed** is settled `Failed` once it has waited
  `MAX_QUEUED_WAIT_HOURS` for the board to hand it out. Asked only of an
  unclaimed row, because `started_at` is the *first* claim: an age read off it
  would call off a run that executed an hour ago.

  The window is **not** the row's whole life, because not all of that wait is
  the card's. It opens at whichever is later, the row's `created_at` or the
  **card's** `updated_at` — the card's clock is what carries a block and the
  unblock that lifts it, since both are writes to the issue row, so a row
  parked by a three-day block is not called off by the very
  `redrive_after_unblock` pass that exists to hand it back out. A `Held` row is
  exempt on its own account: the verdict never asks the age of a row that is
  not `Queued`, because a hold's age is the budget's doing and ageing one out
  would drop work an exhausted board still owes. The parallelism ceiling never
  delays a row this way at all — it gates promotions, not `enqueue`, so a
  recorded row goes to the dispatcher whatever the ceiling says.

  **One wait it over-charges**, stated here rather than closed. The exempt
  hold above stops being exempt the instant it is released, and
  `release_holds` writes a timeline entry and nothing else — neither the
  row's `created_at` nor the card's `updated_at` moves — so a row released
  after a long hold is charged the whole wait the budget cost it. Nothing on
  the row records when it was released, so closing this needs a stored
  instant, not a better predicate. The exposure is narrow: the release
  dispatches the row in the same pass, so only a process death between the
  two leaves it to be aged out on the next boot, and the cost is a `Failed`
  row on a card the operator can retry.

Both give-ups settle **`Failed`**, not `Cancelled`: `driver::newest_run_was_
cancelled` reads a cancel as a stop that stands, so a cancel would keep the lead
from ever being told, while a failure lights the card's badge and the board's
`failed` count — signals that are discharged by acting.

`resumes` is deliberately **not** on `IssueRunDto`. The card's timeline is where
an interruption is said (`RunInterrupted`), and a second copy on the run row
would be a number the board could render disagreeing with the entry beside it.

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

   **And the card says so** (`IssueEventBody::RunRefused`). The refusal is
   correct, but the write that *implied* the run has already committed by
   the time the ledger says no — the card is in its new column, or names
   its new agent, or has just said a stage opened — so without an entry the
   board asserts a change that nothing acted on, and the only trace is a log
   line nobody reads. `RunTrigger::Assigned` exists precisely to stop "the
   board showing @dev-2 on work only @dev-1 ever touched", and the dedupe
   turns it into a no-op in exactly the case it is for: the actor best
   placed to hand a live card over is the one holding its run slot.

   The entry names the **attempt holding the slot**, not just the refusal:
   "refused" is not something an operator can act on and "run #4 still has
   this card" is. It is recorded for every trigger, coordination included,
   with one exception (`runs::refused_itself`): a `Running` holder whose
   `agent_id` is the agent now asking. The intended case is an agent moving
   or reassigning its own card from inside its own turn — the slot is held
   by the run doing the asking, and a comment landing during that run makes
   its settle enqueue the follow-up, so the card has nothing to report. A
   `Queued` or `Held` holder of the same agent's is a real refusal an
   operator can act on, and does record.

   The follow-up is what the exception is betting on, and it does not
   always come: it fires only when somebody commented during the run
   (`wake_after_run` → `was_told_something_during` →
   `comments::somebody_asked_for_more`). A reassignment or a Review → In
   Progress bounce with no comment in the window leaves the card in its new
   column, naming a new assignee, with nothing running under it and no entry
   saying so. What catches it is the lead's `Stalled` question one tick
   later — a billed coordination run for a state `RunTrigger::Assigned`
   exists to prevent. The trade is deliberate: the entry being suppressed
   names the asker's own run as the holder, which is the one refusal an
   operator cannot act on, while "In Progress with nothing running" is a
   state the board already watches for.

   So what the entry counts is every refusal by another run, plus the
   asker's own queued and held ones. Deliberately not a `Comment`: a
   `System`-actored comment satisfies `comments::somebody_asked_for_more`,
   so the settling run would wake its assignee on the board's own note.

   What the entry does **not** do is bring the run back. `append_event`
   never touches `issues.updated_at`, so it re-arms nothing; recovery, where
   it exists at all, is the door's own write moving `driver::last_activity`
   and the lead being asked a `Stalled` question about it — a billed
   coordination run in place of the work, capped at `MAX_ASKS_PER_CARD_STATE`.
   On a swallowed stage barrier there is no recovery to speak of: the entry
   lands on the **parent**, whose `updated_at` the child's write does not
   move, and what the barrier had to say — "stage N opened, drive what comes
   next" — is never re-delivered by anything.
3. **Budget** — `budget::headroom`, which is **two ceilings, not one**: a
   daily money limit and a daily token limit, both optional, both measured
   over the same UTC day and the same rows, and the board stops when
   **either** is reached. The headroom is measured before the write (the
   hold release below needs it), but it never decides *whether* the row is
   written — only what happens to it afterwards. So an exhausted board
   records the work it owes as `Held` rather than dropping it.

   **The two ceilings are symmetric at this gate and asymmetric above it.**
   Neither drops a row here; both hold it. But `stop_runs_over_the_money_cap`
   — the sweep `drive` runs above the promotion gate — cancels the turn under
   every `Running` row on a board whose *money* ceiling is spent
   (`Headroom::money_exhausted`, a narrower question than the `is_exhausted`
   this gate asks; `IssueRunStopper` and `RunStopReason::BudgetExhausted` do
   the stopping). Without it a ceiling bounds nothing: this gate decides
   whether the *next* run starts, and one run's own spend is unbounded, so a
   board overshoots by whatever its longest run costs. A token ceiling never
   stops work in flight — it measures subscription plans, where the turn is
   paid for whether or not it is allowed to finish, so throwing it away buys
   nothing and loses the work.
   That stop writes no `BudgetExhausted` entry, because that entry says the
   run was *held*, which is this gate's doing and the opposite of the sweep's;
   the card learns why from the settle its executor writes, since a cancel
   nobody asked for names its `CancelReason`.

   One read serves both meters: `ProjectStore::spend_since` returns a
   `Spend`, so there is one answer to "what has this board burned today"
   and the two ceilings cannot come to disagree about which cost rows are
   the board's. A board with neither ceiling set still costs nothing — the
   gate short-circuits to `Unlimited` without querying at all.

   **Tokens are `input_tokens + output_tokens`, and the cached columns are
   not added.** `cost_records.input_tokens` is already the whole prompt;
   `cached_input_tokens` and `cache_creation_input_tokens` are subsets of
   it, kept apart only so billing can price the three at different rates,
   and the Anthropic adapter folds its natively-disjoint buckets into that
   convention before the record is written. Summing all four would charge a
   board twice for its cached prefix.

   A hold names the ceiling that produced it — `BudgetExhausted` or
   `TokenBudgetExhausted`, two variants rather than a "which unit" flag
   beside fields called `spent_micros`. When both ceilings are set, the one
   spoken in is **whichever has least room left**, decided once in
   `speaks_money`: exhaustion first, then the tighter fraction,
   cross-multiplied in `i128` so nothing divides and a paused ceiling of
   zero needs no special case. Ties go to tokens.

   Not a fixed "tokens always win" preference, which is what this started
   as and which is wrong in both directions:

   - It named the untouched ceiling on a board held by the other one. A
     timeline row is permanent, so the card would assert forever that a
     limit was exhausted while its own two numbers said it was not, and
     `held_run_refusal` would send the operator to raise the one ceiling
     that would not release anything.
   - Nor is "prefer whichever is exhausted" enough on its own. A release
     happens exactly when **neither** meter is exhausted, so that rule has
     nothing to go on there and would answer a card's hold and its release
     in two different units. Ranking by room left is defined in both
     states, which is what keeps the pair stable.

**The refusals that are knowable before the write have one home**, because
two callers need them there. `enqueue` is allowed to answer `None` and cost
nothing — but a caller that has to make room for the run *first* is not:
the promoter moves the card out of Todo, where no later pass looks for it
again, and the block wake settles the card's only recorded row. So the two
questions that can be answered before anything is written — gate 1
(liveness) and whether the runner can still host a session
(`runs::can_host_a_session`, the ask the team section describes) — are
`ProjectManager::enqueue_refusal`, asked by `enqueue_as` on its way in and
by those two callers *before* the step they cannot take back. Dedupe is
deliberately not in it: that refusal **is** the write, and no caller can be
told it in advance. Budget is not a refusal at all — it holds.

The framework half is the one that bites, and it bites the lead. A board's
`@lead` is an ordinary non-builtin profile, so the admin API can move it
onto `codex` long after the board was opened. Asked on the far side of the
stand-down, the card's only run is settled `Cancelled`, nothing is enqueued
in its place, and the card is left holding no run at all — invisible to the
promoter, the hold release, the boot sweep and the wake alike, which is
precisely the wedge the stand-down exists to prevent.

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

**And when there is no row left to hand back, that door delivers what was
*said* while the card was parked.** A comment on a blocked card is
`ParkedByABlock` — recorded, and nobody put on it — so an answer written
there reaches nobody by itself, while the run that would have read it in a
brief has either settled under the block or stood down for the question that
produced it. The window is from when the block landed (`driver::blocked_at`,
read off the same timeline entry `block_is_an_agents_question` reads, so a
card cannot be adjudicated against one block and re-woken against another),
because that is exactly the interval in which nothing could be woken:
anything older was in a brief, or is the parked row's own to read. Whoever
would run next is skipped, through the same
`comments::somebody_asked_for_more` the mid-run follow-up asks. Without it
the adjudication sits on the card for good and the board's next move is a
Stalled question to the lead — a second billed run in place of an answer
already written down.

**A `Running` row is the third shape, and the delivery above cannot reach
it.** It is the shape the block wake itself produces — the lead lifts the
block from inside its own wake run — and that row is neither handed back nor
re-briefed, while nothing may be enqueued behind it either: the card holds
one unsettled row. So the delivery waits for it to settle, and rides the
follow-up the settle already asks about (`wake_after_run`), with the window
widened from the run's brief back to the block that stood when it was
briefed (`driver::block_standing_at`, asked only on a settle whose own window
holds an `Unblocked` — so an ordinary settle costs no second read). Read of
the moment rather than of the newest block, so a card unblocked *before* the
run was briefed answers `None`: that window was handed over by the unblock
itself, and re-delivering it would wake the assignee on the answer it has
already been given.

An unblock that **also** starts a run hands out one run, not two.
`update_issue` runs the trigger first and passes the row it recorded to
`redrive_after_unblock`, which then knows the card's one unsettled row is its
own doing: a row written after the block was gone was never parked by it, and
cuts a brief that reads the same comments a hand-back would have handed over.

At a **process start** the work is split in two. `requeue_unsettled` rolls every
`running` row back to `Queued` in one statement, bumping each row's `resumes` as
it goes, and answers with the rows it rolled back — an orphan is an orphan
whatever board it is on, an archived one included, and rolling a row forward
settles nothing. `resume_unsettled_runs` writes one `RunInterrupted` entry per
returned row, archived boards included: `record` is one of the writes that never
asks `writable_project`, because it describes work already under way. The
give-up bound is applied per board afterwards, which *does* ask, so a shelved
board's over-limit row waits for the restore rather than being settled on a
shelf. `resume_project_runs` then walks the boards, each reading its own
unfinished rows, so runs go out per board in issue order, oldest card first,
with no order promised across boards. Nothing downstream depends on which run
starts first. That sweep is spawned *alongside* the server rather than before it
(`gateway_cmd.rs`), so it races live traffic: a restore landing in the instant
before the requeue commits finds its orphans still `Running`, skips them, and
they wait for the next process start.

Nothing renders "interrupted" on the card *face*, deliberately: the row reads
`queued` with a session on it, and adding a run state to the board's status
vocabulary is a change on both sides of a hand-written mirror. The timeline is
where an interruption is said. A card showing "running" straight through a
gateway outage cannot be fixed at all — nothing is serving that page.

`retry_run` is the one caller that refuses a finished card itself rather than
letting `enqueue` do it silently. `enqueue` can only answer `None`, which
`retry_run` reads as the dedupe guard, so the operator would be told the card
already has a run when it has none and never will. It refuses a **blocked**
card for its own reason, and in that order: nothing clears `blocked_reason`
when a card is finished, so a Done card keeps one for good and would otherwise
be told to lift a block that is not why it was refused.

### Giving the build output back

A finished card's whole checkout goes (`reclaim_if_finished` → `worktree::reclaim`). A card that is merely *idle* keeps it, deliberately — the branch is still being looked at — and that is where the disk goes: two cards parked in Review held 5.66G of `target/`, every byte of it reproducible by running the build again.

`artifacts.rs` owns which of those bytes may go, and hands the answer out as one verb, `BuildArtifacts::reclaim_idle_build_artifacts(idle_for)`. The maintenance loop that calls it (`baybo-janitor`, every 12h at a 3-day TTL) supplies the cadence and knows nothing else; the adapter between the two lives in the composition root, so neither crate depends on the other.

Four gates, and all of them have to agree:

- **Nothing owed.** `between_runs` — a `Held`, `Queued` or `Running` row all hold the issue's dedupe slot, and any of them means something is about to build in that tree.
- **Idle for real.** `checkout_last_touched` is the card's `updated_at` or its newest run's settle, whichever is later. Deliberately *not* the driver's `already_asked` activity, which filters coordination runs out: that predicate asks whether the card changed since the lead looked, this one asks whether a shell ran in the tree, and the lead's wakes get a checkout like every other run.
- **A name a build tool owns**, from an explicit list (`target`, `node_modules`, `.venv`, `__pycache__`) — not "anything git ignores", because a `.env`, a downloaded fixture and a half-written scratch file are all ignored too and none of them comes back. `dist/` and `build/` are absent on purpose: those can be the artefact the card exists to produce.
- **git agrees**, per directory (`check-ignore`), so a repository that tracks one of those names keeps it.

Then the path is canonicalised and re-checked for containment on every sweep (`admitted`), which is the c957e790 rule asked here for the reason it is asked on every tool call: the agent that worked this card can write inside `work/`, so it can turn any component of that path into a link somewhere else between one sweep and the next, and a recursive delete that trusted the layout would follow it.

The result is counted, never announced on the card: a rebuilt cache is not something the operator has to be told about, and the janitor's own report is where a sweep says what it did.

### What a run cost

A run's spend is **derived, never stored**: `RUN_COST_WINDOW`
(`sqlite/project.rs`) attributes a `cost_records` row to the run whose
claim→settle window on that session contains it, and the two readers —
`run_spend` (a card's execution log) and `settled_run_facts` (the board
feed's `run #1 done on #7 · 2m10s · $0.04`) — are the same predicate under
two addressings. Reach them through `ProjectManager::run_log` /
`ProjectManager::feed`; a caller that joins `issue_runs.session_id` to
`cost_records` itself over-counts by a factor of however many attempts
share the session (3.3× on a real board), because that is what a session
*is* here.

There is deliberately no `cost_records.run_id`. The id is not reachable
where the row is written — the ledger sees an `Attribution` of
user/session/turn/span and has never heard of a board — so the column
would be NULL for every existing row and every reader would need the
window anyway, which is two homes for one rule.

The window is unambiguous **only** because of two invariants that live
elsewhere, and it silently double-counts if either is relaxed:

- `idx_issue_runs_live_agent` permits at most one unsettled run per
  (issue, agent), so two windows on one session cannot overlap.
- `Router::issue_session` mints one session per card *per agent*, so a
  session never spans two cards.

The first of those is deliberately **not** `idx_issue_runs_live`, which is
the card's own slot and is wider. The two are the same constraint today —
one live run per card implies one per agent on it — and are written down
separately because only the narrow one is what this window and the run
waiter need. Widening the card's slot, which is what any lane scheme does,
must not silently take the narrow guard with it; the redundant index is
where that is caught, and a runtime check could not stand in for it,
because it could never fire while the wider index refuses first.

Allow one agent two live runs on a card, or one session across cards, and
`cost_records.run_id` becomes the right answer.

`started_at` is therefore load-bearing beyond the timeline: it is the
window's left edge. The process-start requeue leaves it alone and
`claim_run` re-stamps it only when it is absent (`COALESCE`), so a run the
daemon died under keeps everything it had already spent. The cost of that
is a duration spanning the downtime — the honest number for a window that
bills those hours.

Two things the window does not see, both named here so they are not
rediscovered as bugs: a **subagent** spawned by a run bills against its own
session id, so it is invisible to `run_spend` *and* to both of the budget
gate's meters (widening them together, via `sessions.root_session_id`, is
the fix — widening one alone would make a card's total exceed its board's).
That hole is *worse* under a token ceiling than under a money one, because
on a subscription board tokens are the only meter that moves at all, so a
subagent-heavy run is invisible to the only ceiling that can fire.

And a run **priced at $0.00 with real tokens is the ordinary case, not an
unreachable one** — this paragraph used to claim it was gated off by
`can_host_a_session`, which is wrong and is why the hole went unnoticed.
That gate asks about the agent's *framework* (`runs::accepts_runs` →
`AgentFramework::Baybo`); it says nothing about which LLM entry the agent
runs against. A teammate whose `profile.llm` names an `openai-subscription`
entry is an ordinary baybo agent, and every one of its calls prices at zero:
`openai-subscription` has no OpenRouter prefix, so
`openrouter::pricing_for` misses, and the factory ships
`ModelPricing::default()` deliberately — subscription billing is
account-level, not per-token. `spend_since` therefore sums `cost_usd = 0`
forever and **a money ceiling on such a board can never be reached, however
low it is set.** The token ceiling below is the answer to that; the rejected
alternative was a hand-written per-entry `pricing` override, which is a fake
price for money that does not exist.

The class is wider than one provider: anything absent from
`openrouter_prefix.rs` that does not override `flat_default_pricing` prices
at zero the same way.

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

An exhausted board records work instead of dropping it. The card says so
twice — `held` where its live run's state is drawn, and a timeline entry
carrying the figures (`BudgetExhausted`) — and it starts the moment there is
headroom again. That is what the board does with work it has not started; a
run already executing when the *money* ceiling goes is stopped instead, by
`stop_runs_over_the_money_cap` rather than by anything in this section.

Holds are released **by activity, and once a tick by the driver**. Five doors
reach `release_holds` on activity: a budget change, the boot sweep, a board
coming back off the shelf, an unblock that finds a held row, and every enqueue
that gets past the liveness gate onto a card with somebody on it — which is
every enqueue that was going to write anything. (An enqueue the liveness gate
refuses releases nothing; the board's other holds wait for the next thing that
happens on it.) The sixth is the driver: `promotions` releases unconditionally
at the top of every `DRIVE_INTERVAL` pass over a board that is neither archived
nor set to zero parallelism. That is what covers the one thing no activity
announces — the UTC day rolling over. Why that release sits *before* the pass
counts slots is in the driver section below.

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

"Most urgent first, then column order" is `driver::promotion_order`,
`(priority, position, number)`, and it is the same order `IssueList` reads a
column in — "what is next in Todo" has one answer whether an agent asks or
the board acts. `issues.pinned` is **not** in it. A pin is how the operator
wants a column *read* (the board floats a pinned card to the top of its
column, and nothing else); `priority` is what the board should work on
first, and two fields answering that question is one of them being wrong.
Pinning a card to keep an eye on it must not quietly promote it past urgent
work. The pin also writes no timeline entry, for the same reason priority
does not: it changes nothing about the work.

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

This is *not* the shape a hold's release is mostly driven by — that one is
edge-driven off activity, and the difference is worth knowing: a hold only
matters to the card holding it, so the next thing that happens on that board
releases it. A promotion is about the board's spare capacity, which the card
that would use it has no way to observe. The tick releases holds too, and the
paragraph below is why: one event on a budget-limited board is announced by no
activity at all.

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
budget-limited board is holding runs. Both ceilings share that one window
(`budget::day_start`) precisely so there is only one such instant; two
windows would roll over at different times and leave a board over one limit
and under the other for a reason nothing on screen could explain.

At a process start the driver is sequenced *after* `resume_unsettled_runs`, in
the same task (`gateway_cmd.rs`). They cannot be folded into one function —
`requeue_unsettled` rolls every `running` row back to `queued`, which on a live
board would orphan the runs actually executing, so it is once-per-process and
the sweep is not. But racing them in two tasks leaves a re-dispatch and a
promotion to be sorted out by `claim_run` alone, and there is no reason to
spend that guard here.

Five things it will not do, each of which is a rule and not a coincidence:

- **It will not promote a card whose assignee cannot host a run.** Asked in
  `promote` through `enqueue_refusal`, *before* the move — and that ordering
  is the whole point. The two
  writes a promotion makes are a move and an enqueue, in that order and not
  atomic, so every reason the enqueue could refuse has to be settled first: a
  card moved into In Progress and then refused a run is stranded, because it is
  no longer in Todo for a later pass to find. `is_promotable` only knows that
  *somebody* is on the card; whether that somebody still runs on baybo changes
  under the board's feet, since an operator can move an agent to another
  framework long after it was assigned. The block wake makes the same two
  writes in the same order — a stand-down and an enqueue — which is why the
  question is one predicate and not a line copied into the second caller.
- **It will not promote a card that already has a run recorded.** A card holds
  one run slot; promoting one that is spoken for would move it into In Progress
  and then fail to start anything, which is the one state that column must never
  be in. This is not an edge case — a comment on a Todo card wakes its assignee
  where it stands, and a triage run sits on an unstaffed card by construction.
  Both are in the busy set, held runs included.
- **It will not overrule a block.** A person dragging a blocked card into In
  Progress is overriding the block deliberately; the board doing it would be
  overriding it on nobody's authority. `driver::board_may_start` is that rule,
  and the promoter is not its only reader: the hold release, the boot sweep,
  the stage barrier, `runs::verdict`, the mention inside a comment
  (`mention_assignment`), the operator's own `retry_run` — which names no
  field of the card, so it is not one of the two writes that override a block
  — and, because the block wake's own brief offers "hand it back with a
  comment", `comments::comment_delivery` all ask it.
  The comment door is the one that has to be named out loud, because a
  comment is otherwise the board's ordinary way of waking an assignee: without
  it the lead answering a block was the thing that started work against it.
  Its `@mention` half needed naming separately, because an assignment reaches
  a run through `triggers_run` rather than through the delivery, so the gate
  on one says nothing about the other.
- **It will not start work while either ceiling is exhausted.** `enqueue` would
  record the run and hold it, leaving a card in In Progress with nothing running
  under it. The existing hold/release path already owes that work. Stopping
  work is the other half and not this gate's: `stop_runs_over_the_money_cap`
  runs earlier in the same pass, above the promotion gate and only for money.
- **It will not preempt.** Priority decides who gets the *next* free slot, not
  who keeps one. A card already running keeps running when something more
  urgent arrives.

Ordering is `(priority, position, number)` — the same order `IssueList` already
reads a column in, deliberately, so "what is next in Todo" has one answer
whether an agent asks or the board acts. Note that the **web board renders in
`position` order with pinned cards lifted first and unread cards lifted within
each partition** (`readingOrder`, a reading order that writes nothing), so on a
column with mixed priorities the card the board takes next is not necessarily
the one rendered at the top.

**Asking the lead.** Some cards are not work the board can start — they are
questions only the lead can answer, and the same pass that promotes asks them
(`ask_the_lead`), one card per pass because the lead reads the whole board when
it is woken. One card **that was actually asked**: a candidate whose enqueue
answers `None` woke nobody, so the pass goes on to the next one. Returning
there instead let a single card the board can never start a run on — a
finished one still carrying a block reason, an agent moved off baybo —
swallow every question behind it, on every tick, in silence. Five questions,
in the order they matter, each its own trigger so the execution log says which
was asked (`RunTrigger::is_coordination`):

- **Blocked** — a card a block has stopped, whose reason is an **agent's**
  question rather than a person's stop. It goes first: review and stalled work
  are revisited by other machinery, whereas a blocked card is invisible to the
  promoter, to the hold release and to the boot sweep, so if nobody is woken
  about it, it is the one card on the board nothing ever comes back to. This
  is also the only question that requires what every other door refuses, which
  is why `board_may_start` lives on each of the other three rather than inside
  `takes_a_lead_question` — folding it in would let the block silence the one
  wake that exists to answer it. `runs::verdict` exempts `RunTrigger::Blocked`
  from the block park for the same reason, and **only** that trigger: a
  Review, Stalled or Triage row recorded before the block asks about something
  else, and handing one out would put the lead on a card somebody paused. Who
  wrote the block is not on the row, so `driver::block_is_an_agents_question`
  reads it off the timeline — the same shape as the stall question's
  `newest_run_was_cancelled`, asked by the caller once it has the entries in
  hand, and asked **last** because it is the only gate that costs a second
  read. An operator's own block wakes nobody: they said stop, and adjudicating
  it would countermand them within one tick.

  **Two things had to give way for this wake to be reachable at all**, and
  both are the block silencing its own question:

  - *The card is not busy just because it holds a row.* The lead's questions
    are asked against an `in_flight` set, not the promoter's `busy` one: a
    row `runs::parked_by_a_block` has stopped is not work in flight, since
    nothing hands it out until the block lifts and only this wake lifts it.
    The two sets differ on blocked cards and nowhere else, so the other three
    questions read the same either way. A row that is still `Running` is not
    parked — the block landed under a turn whose executor will settle it — so
    a card being worked stays the executor's.
  - *The parked row stands down.* `idx_issue_runs_live` allows one unsettled
    run per card, so the parked row and the question cannot both exist. Past
    the agent's-question gate — **and past every refusal the enqueue can
    make**, since the settle cannot be taken back and the wake that would
    replace it is not yet a fact — `stand_down_for_the_question` settles the
    parked row `Cancelled` with the reason on the card, and the wake takes
    the slot. The work loses nothing it could keep: the row was not
    executing, and whatever runs after the adjudication continues the same
    session, which `session_run_to_continue` keys on the agent rather than on
    the row. An operator's block never reaches this — a person's stop still
    parks its run for `redrive_after_unblock` to hand back out unchanged.

    A **held** row stands down as well, and that is decided rather than
    incidental. Its other exits are all shut: `release_holds` refuses a
    blocked card, and the only thing that lifts the block is the question
    the hold is silencing — so a hold left standing is a card with no exit
    at all, which is worse than losing the row. It also loses less than the
    queued row beside it: a hold never executed, never opened a session and
    never spent anything, and the answer it is waiting on is what would
    have re-instructed it anyway.

  A **finished** card is not a candidate either (`runs::accepts_runs`, asked
  in `driver::blocked` because the other three pin their own column). Nothing
  clears `blocked_reason` when a card is closed, so a Done card keeps a stale
  reason for good — and this question is asked first, on every tick.
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
- **Grooming** — a card parked in **Backlog** that an **agent** filed there.
  Asked last, because everything above it is about work already under way and
  this is about work that has not started.

  Backlog is the one live column the board **pulls** nothing from and still
  **asks** about, and both halves are deliberate. `driver::is_waiting` opens
  on `Todo`, so a card in Backlog is work nothing will ever start on its own;
  when a *person* put it there that is the column doing its job, and the board
  reopening it would be overruling them the same way adjudicating their block
  would. When the **board** put it there it is a dead end, and before this
  question existed it was the board's only unreachable state: with Todo, In
  Progress and the non-lead half of Review all empty, every remaining wake is
  downstream of a card sitting in a column the driver reads, so a board that
  finished its last card went quiet with no error, no log line and no badge,
  until a person commented on something.

  Authorship, not the assignee, is what tells the two apart, and it lives only
  on the timeline — the same shape as `block_is_an_agents_question`, but asked
  as **one query per board** (`ProjectStore::agent_opened_issues`) rather than
  a timeline read per card: this question is asked every pass, and a Backlog
  full of the operator's own cards would otherwise cost a full event list each,
  every tick, forever. A card with no `Opened` entry at all — older than the
  entry — counts as nobody's, which leaves it parked.

  Deliberately **assignee-agnostic**, unlike Triage. A staffed Backlog card is
  not work waiting for a slot; nothing is coming for it either. Asking only
  about the unstaffed ones would strand precisely the cards a lead had already
  thought about.

  Asking is not moving: the wake hands the lead the card, and only the lead
  moves it to Todo — where the ordinary promoter takes it on the next tick, as
  `RunTrigger::Promoted`, run by its own assignee.

  The card face says which kind it is. `BoardCards` carries the same answer
  through to `IssueDto.opened_by_agent`, resolved once beside `CardSignals`
  rather than left to whoever draws a card — an operator looking at a Backlog
  column is looking at two rules, and a face that re-derived authorship could
  mark a card the board will never ask about. It sits on `BoardCards` and not
  in `CardSignals`, whose map is sparse on purpose: absent there means
  "nothing waiting", and most cards on a working board are an agent's.

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

### Two card-to-card edges, and only one of them schedules

The board holds two, and conflating them is the mistake the second one exists
to avoid.

`parent_issue_id` is **hierarchy**: a planned step, one level deep, that arms
the stage barrier below. It is the reason the board is a list of cards and
their steps rather than a tree nobody can read at a glance, and that claim is
about hierarchy alone.

`filed_from` is **provenance**: the card whose run opened this one. Unbounded
in depth, it gates nothing, wakes nobody, and appears in no ordering. It is
derived from the calling session by `tools::filed_from` — never a tool
parameter, never a request field — because a session working a card already
carries which card with certainty, and a model asked to restate that can
forget it, mistype it, or reach for `parent` instead. Cards opened from a cron
fire and from the operator's own create door carry none: they are roots, which
is the truth and not a gap.

It is written once, in `create_issue`, and is deliberately absent from
`IssueUpdate` and the REST patch. Numbers are `MAX(number) + 1` per board, so
a write-once edge to an already-existing card always points at a smaller
number and the relation is acyclic by construction — nothing here detects a
cycle, and making this patchable would be the change that first needs one.
The origin is resolved through `issue_by_id`, which is project-scoped, so one
board's card can never be another's origin.

Both directions are readable. The card wears `↳ #N` on its tile, which is the
scanning question — a Backlog where four of six cards are debt spun out of
finished work draws them identically to the two that were planned. The origin
gets an `IssueEventBody::Filed` entry, which is the direction nothing else
answered: without it a Done card falls silent at the moment its review spun
out three more. A dedupe hit (`Opened::AlreadyOpen`) opened no card and so
files none.

`Filed` **is** in `timeline::left_a_mark`, which is what a run that died
mid-flight is judged by: a run whose whole output was three follow-up cards
did not leave its card untouched, and `RUN_LEFT_NOTHING_ON_THE_CARD` would
send the operator looking for work that is already on the board. It does
**not** light the unread badge — that is `UNREAD_EVENT_PREDICATE`, a separate
whitelist of comments, blocks and arrivals in Review — because the operator
filed nothing and has nothing to answer. The two are different questions and
this entry answers them differently on purpose.

The one thing a scalar cannot hold: a description routinely names an origin
the filing card is not ("#12 验收 follow-up（原卡 7）"). The field is named
after the mechanism the board can vouch for — which card's run filed this —
and the editorial claim of cause stays in the prose, where it can name two
origins and a reason. Provenance here is a forest of filing edges, not a
causal tree.

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
| A block has stopped the card | `ParkedByABlock` — recorded, and nobody may be put on it |
| Live work, nothing reading | `Wake` — start a run |
| A `Held` or `Queued` run exists | `WaitsForQueuedRun` — it assembles its brief later, so it will read this |
| A `Running` run exists | `AfterCurrentRun` — deferred |

`ParkedByABlock` is `driver::board_may_start` asked at the comment door, and it
is ahead of the live-run rows deliberately: a `Running` run promises a
follow-up, and on a blocked card that follow-up is a run the block refuses. It
is told apart from `RecordOnly` because the two are answered differently — one
is a card nobody is working, the other is a card with a named reason on it and
a decision waiting to be made.

The deferred case is the one with a moving part, and it is the board's to
resolve, not the executor's: `finish_run` asks `wake_after_run` once the run
settles and the issue's live-run slot is free again, and that goes through
`enqueue` like every other start. Writing the ledger row directly would produce
a run nothing ever dispatches, holding the slot until the next boot.

Three things bound "somebody said something", and they are the reason the
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
- **Unless the card came out of a block while the run worked**, in which case
  the window reaches back to when that block landed. Everything said under the
  block was answered `ParkedByABlock` — recorded, and nobody put on it — and a
  run briefed under the block did not read it either, so the run in flight when
  the block lifted is the first thing that can hand it over. This is what makes
  the block wake's own shape work: the lead lifts the block from inside its
  wake run, and the answer written while the card was parked reaches the
  assignee when that run settles rather than sitting on the card until the
  board bills a Stalled question about it.
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

**And never on a card a block has stopped.** `ParkedByABlock` is "recorded,
and nobody may be put on it", and an assignment is not only a staffing edit:
on a card in In Progress it is `triggers_run`'s second edge, so a mention
that walked past the block would start the very work the block stopped —
through the one door that never asks `board_may_start`. The gate sits at
this door (`mention_assignment`) rather than inside `mentions::assigns_to`,
which stays the pure "is this comment a handover" rule the web mirrors, and
beside the other card-level refusal the mention already answers to (an agent
that cannot host a session).

Both of those refusals are **log lines and nothing else** — the comment
itself lands, so the card records the words and says nothing about the
staffing they asked for. That is the reason `mentionHint` carries the block:
the composer refuses in advance, in front of the person typing, which is the
only place a refusal that writes nothing can be seen.

That leaves `dispatch_if_triggered` — the last place `board_may_start` is
not asked — exempt through exactly two doors, and both are an explicit
write naming the card's status or its assignee: `move_issue` and
`update_issue`. A person dragging a blocked card into In Progress is
overriding the block deliberately, and the block stays on the card while
they do it, so the card still reads as paused and the lead is still asked
about it. Both doors are reachable by an **agent** too, through the
`IssueUpdate` tool — the override is not the operator's alone. What makes
that acceptable is the same thing that makes the drag acceptable: the write
names the field, the `Moved`/`Assigned` entry it produces carries the
agent's own name, and the block is still on the card afterwards.
`create_issue` reaches the same trigger and cannot be an override — a card
cannot be opened already blocked.

Every other door asks the gate, the board's own and the operator's alike:
the promoter, the hold release, the boot sweep, the stage barrier,
`runs::verdict`, the comment door, the mention inside it, the lead's Review,
Stalled and Triage wakes — and `retry_run`. That last one is an *operator*
door and still asks, because the two exemptions are exemptions for naming a
field: "run it again" names none, so a run started that way is a block overruled
with nothing on the card recording that anybody decided to. It refuses with
`RETRY_ON_A_BLOCKED_CARD` instead, which names the one write that lifts it.

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
| `start_run(run, session)` | Claims the row, announces the row's move to `running`, then says `RunStarted` on the card **once** | The claim is what stops two agents on one card, so nothing may say a run started without having won it. A re-claimed row is already on the card — the interruption between the two claims is the sweep's `RunInterrupted`, not a second "started". Only the *entry* is deduplicated: the state change is announced either way, or a resumed run renders as `queued` for the whole of its second life |
| `finish_run(run, checkout, briefed_at, outcome)` | Settles, surfaces the branch, then follows up on comments | The order is a rule: the branch is read before the follow-up because a follow-up enqueues another run against the same checkout, and the settle is first so a card whose branch cannot be read still stops shimmering |
| `run_left_a_mark(run, briefed_at)` | Answers whether this run touched the card after it was briefed | The executor may say a run *failed* — it watched the turn — but not that it produced *nothing*, because it cannot see the card |

`briefed_at` is handed straight back from `IssueRunEvent` — the executor carries
it rather than deriving it, because it is the only record of when the run's
brief was read and the ledger row's own two instants both fall on the wrong side
of it. See the follow-up window below.

`RunOutcome` is the whole of what the executor decides — it watched the turn, so
`status`, `error` and `stopped_by_a_human` are its answers — and what those cost
the card is this crate's. `stopped_by_a_human` is separate from `status` because
the ledger row cannot carry it and it changes what the board owes: somebody who
pressed stop is not asking for a follow-up.

**One negative it may not assert.** "This run produced nothing" is a claim about
the *card*, and the executor cannot see one: a run that committed, commented and
moved its card to Review seconds before an interrupt was settled with that
sentence, and the lead — reading a card that flatly contradicted itself — wrote
an invented root cause into a permanent timeline. `run_left_a_mark` is the
board's answer, over the run agent's own entries since `briefed_at`, and the
executor picks its sentence from it. Bounded to the timeline on purpose: a
commit is surfaced by `record_branch` either way, and asking git would put a
second shell-out on a settle path. A read that *fails* answers "it worked" —
asserting a negative off a failed read is the same defect.

Which is why the sentence it picks says only what the board checked: **nothing
this run did reached the card**, and its branch may still hold work. A run that
committed without commenting or moving anything leaves no timeline entry at
all, so a flat "produced nothing" would be the same invented negative one
altitude down — the incident's own run had pushed a commit.

**The settle reason reaches the card**, because the lead reads the card and not
the transcript. The executor reads the whole `TurnStatus` for every terminal
kind, not only for a cancel: a provider's own failure text lands on the card
(clipped to `MAX_RUN_ERROR_CHARS`, since it is permanent and read on a card
face), and a cancel nobody asked for names its `CancelReason` instead of
arriving as a bare "cancelled" — which is exactly the blank a lead fills in with
a guess. A person's own stop still carries no note: they know why they pressed
it.

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

The card's status, non-default priority, assignee and branch render on one
properties line. Those are the current facts a run otherwise had to recover by
calling `IssueGet` on the card it had just been handed. `IssueGet` remains the
door for system events, omitted comments, another card, or a brief that has
scrolled out of reach. The runtime brief states those exact exceptions before
the card, and the tool description repeats them at the decision point; neither
claims the brief holds the whole timeline. Project-agent SOUL seeds carry only
durable board and role invariants; current-card and fetch instructions live
entirely in runtime framing instead of an agent's editable identity.

A card that carries a `blocked_reason` renders it, for **every** trigger — a
block is a standing fact about the card, not a fact about one wake. The lead's
block preamble says "read the reason and decide", so the reason itself has to
ride beside the other current card facts.

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
`started_at`: the executor stamps both when it claims a run, and both survive
the process-start requeue — `started_at` deliberately, because it is the left
edge of the run's spend window and `claim_run` re-stamps it only through
`COALESCE`. The session is what the three questions below are actually about,
so it is what the predicate reads. This crate asks the same predicate
for a different reason — a run being called off after a restart is told it was
interrupted, not that it never started. A row that never claimed a session never
opened a transcript and never touched the checkout, so none of the three
questions — whose session this run continues, whose uncommitted changes are
waiting in the tree it is handed, what its call-off says — counts it at all.

Issue run sessions do not appear in the global chat list; their transcripts are
reached through their cards.

### The team

A project's team is `agent_profiles` rows carrying a `TeamMembership`
(`project_id` + `handle` as one field, never two nullable columns that could
disagree). Every board opens with a `@lead`, seeded **before** the project row so
a failure leaves an inert orphan rather than a visible board with no
coordinator.

**And a board that predates that invariant is repaired into it.** `lead_of` is a
string comparison against `LEAD_HANDLE` that silently answers `None`, so a board
opened before the seed existed — whose operator, handles being permanent, hired
its coordinator as `@leader` and cannot rename it — had every coordination run
refused by a comparison nothing logged, and never asked a single question. The
driver therefore goes through `coordinator`, which seeds a lead on a miss rather
than returning `None`: `seed_lead` is idempotent-by-conflict (the unique index
refuses a second `lead`), it does not consult `MAX_TEAM_AGENTS` (the lead comes
with the board, it is not hired), and `ensure_named_persona_layout` is safe to
re-run. A board whose lead was *removed* keeps that handle reserved forever, so
the seed fails there — that case warns **once per process**, through a
`leadless` set, and names the fix. The set is consulted only after a seed has
already failed, so an operator who hires an agent called `lead` by hand is
picked up on the next tick without a restart.

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
  `validate_assignee` when the card is assigned, before a row is recorded
  (through `enqueue_refusal`, which is `enqueue_as`'s own gate and the
  pre-flight the promoter and the block wake ask ahead of the irreversible
  step each makes), and by the executor's `binding_for` before the answer is
  written into a
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
deny-on-timeout. Installed once over the channel, not per run.

**The prompt is named for the session that raised it, never for the card.**
`asker` reads the actor off `SessionState::agent_id`, which the router writes
once when it mints the issue's session. Reading `issue.assignee` instead — which
is what it did — gets the wrong agent in the ordinary case, not an exotic one: a
coordination run executes as the board's `@lead` by construction
(`driver::takes_a_lead_question` refuses a card the lead is already on), so
every prompt a Review, Stalled, Blocked or Triage run raised was announced under
the assignee's name. The binding is also the only one of the two that cannot
drift — the assignee moves under a live run every time somebody is handed the
card, while a write-once binding answers for the whole session. It costs one
fewer store read as well: the session was already loaded to find the card.

**Three closers, one claim.** The gate keeps an in-memory ledger of the prompts
this process has announced and not yet resolved. Taking an entry out of it *is*
the claim on writing that prompt's resolution, so exactly one of three closers
can write a given `call_id` and the timeline can neither dangle nor
double-close:

1. the gate's own return, when the inner gate answers (or denies itself on
   timeout);
2. `CardPromptCloser::close_card_prompts`, the port the run-settle path holds so
   a run that stops being able to answer a prompt takes the prompt with it;
3. `TimelineApprovalGate::close_open_prompts`, called from
   `ManagerGraph::shutdown` **before** the actor cancel cascade.

That ordering is load-bearing. A per-request `Drop` guard still exists and still
covers an ordinary turn cancel, where the runtime is live and its spawned write
lands — but `Drop` cannot await, so on a runtime already past its task drain the
write is simply lost. That is how one `rm -rf` prompt on issue #3 sat in the
ledger with an `approval_requested` row and no resolution under it from the day
it was raised. Reversing the two statements in `shutdown` re-creates exactly
that path.

Closer 2 is a **chokepoint**, and in two senses. `settle::settle_run` calls
`close_card_prompts` right after the `RunSettled` entry — inside the settle
itself rather than at its three call sites, because a prompt outlives its run
whenever a caller forgets, and the run is what was going to answer it. And the
board is *handed* the closer by `TimelineApprovalGate::new` rather than being
wired to it by whoever installs the gate: a port somebody has to remember to
connect is one this crate already shipped once with no callers at all. The
dispatcher's own settle passes `None` — a run whose checkout never opened
reached no tool and raised no prompt.

**Closing a prompt ends the request, not just the record.** Each ledger entry
carries the `oneshot::Sender` that un-parks the wrapped gate's own call, so
taking the entry drops it, and the wrapper — selecting on that receiver against
`inner.request` — drops the inner future, which is what takes the prompt off
the channel's queue (its `QueueCleanup` guard). The claim then arbitrates the
**answer** as well as the ledger line: a request whose entry somebody else took
returns `abandoned` whatever the inner gate says. A write-only closer left the
human able to press Allow afterwards, and the tool ran against a card recording
that nobody had allowed it.

Pending-ness is **derived**, never stored: a request with no matching resolution
on the same `call_id` is still open. A `pending` flag would be a second copy of a
fact the timeline already carries, and the two would eventually disagree. The
claim ledger is not that flag — it is in-memory process state about what *this
process* announced, and it says nothing after a restart.

`ApprovalResolved` carries the resolution as well as the decision, because a
human's "no", an expired window, an abandoned prompt and a standing policy are
four different facts that arrive as one `Deny`.

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

### Attention: three signals, and the one that was thrown out

`attention()` answers one question per board — is anything waiting on the
operator — as three counts: `approvals`, `failed`, `unread`. The membership
rule is that **every one of them is an event**: something arrived, broke, or
is being asked. They then divide on what discharges them:

- `approvals` clears by **acting**. Answer the prompt. Looking at it changes
  nothing a query could see, which is exactly why it needs no stored state.
- `unread` clears by **reading**.
- `failed` is **both**, and is the only signal read by two rules. The *card* is
  broken until somebody acts — retry, finish, cancel, block — and wears its
  badge that whole time; nothing retries by itself, so a badge a glance could
  clear would take the board's own record of what is broken with it. The
  *rail's* mark is only a pointer, and a pointer that survives being followed
  is noise: it stays lit after the operator has read the failure, with nothing
  left to do about it but act on a card they may be deliberately leaving until
  tomorrow. So the board's count drops a failure once that card's cursor has
  moved — opening it, or reading the whole board — while the card goes on
  saying so. A card that fails *again* relights the rail off the same cursor —
  one rule, not two.

**Runs the daily ceiling is holding are deliberately absent**, and the reason
is the membership rule rather than any doubt that they matter — a stopped
board is the most literal "only you can fix this" there is. A hold is a
*standing condition*: it does not arrive, and it stops being true only when
the operator changes a number. Rendered into the rail's one undifferentiated
dot — the same red the card's unread pill wears — it was indistinguishable
from a mark that could not be cleared at all, and that is precisely how it was
reported. So a condition now goes where it can be acted on: `OverCeilingChip`
in the board header's own action group, carrying the spend against the ceiling
as a figure on screen and the runs it is holding in its title, and opening the
setting that lifts it. The rail keeps the events.

What that costs is cross-board reach — a board frozen by its own ceiling no
longer says so from the rail, and you learn it by opening the board or the
project switcher, whose per-board meter shows `602k / 100k` in the warn tone
(`burnState`). That was the accepted trade, not an oversight.

The cursor is `issues.read_at`, one per card, moved by `mark_issue_read` and
`mark_project_read` and by nothing else. Per card and not per board: an
operator who reads the question asked on #3 has not read the one asked on #7,
and the board-level stamp this replaced could only clear both or neither — it
was fired by the board page's load effect, so it also swallowed everything
written between that page's fetches and the POST landing.

`mark_project_read` does not put that stamp back. What made the old one a bad
trade was that it was **automatic** — merely arriving on the board discharged
every question on it. This one is an act the operator asks for, in one press,
on a board in front of them, and it writes the same per-card cursors: a
board-wide `read_at` column would still be a second cursor free to disagree
with the first. Every card on the board is stamped, cancelled and finished ones
included — the cursor says "seen", and a card being over is not a reason to go
on counting what was said on it. The store answers with the rows it moved,
which is what makes the monotonic guard testable; like `mark_issue_read`'s
`bool`, nothing above the store reads it.

Two consequences worth stating, because one press reaches further than
"unread" sounds like it does. It clears the board's `failed` count as well —
`UNSEEN_FAILURE_PREDICATE` rides this same cursor, so a failure nobody has
opened stops lighting the rail (the cards keep their badges; this is the
divergence below, in its documented safe direction). And it reaches cards the
operator cannot currently see, since the board's filter is a client-side view
and the stamp is not; the button says so on hover.

Three SQL predicates in `crates/storage/src/sqlite/project.rs` are the single
home of these rules:

- `UNREAD_EVENT_PREDICATE` — an agent's comment, an agent blocking the card, or
  an agent moving it into Review, newer than that card's `read_at`. The actor
  filter covers all three arms: the operator's own words, their own block and
  their own tidying are not news to them. An agent's block joins the other two
  because it is a decision the operator did not make, and on a blocked card it
  is usually a question that gates the work.
- `FAILED_CARD_PREDICATE` — a live card whose newest run failed. Both
  `card_signals()` (the badge) and `attention()` read it.
- `UNSEEN_FAILURE_PREDICATE` — and that run settled after `read_at`.
  `attention()` alone adds it; the card's badge must not.

The two failure predicates read the same run by construction: the `newest_run!`
macro is where "newest" is spelled, so `status` and `settled_at` cannot come to
be read off different rows.

`card_signals()` reads per card and `attention()` per board, so on any live
board the `unread` count is the sum of its cards' and the `failed` count is the
number of cards wearing the marker **whose cursor has not moved since they
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
on an exhausted ceiling — either of the two — and `promotions` returns early on
`max_parallel_issue_runs == 0`, so every deliberate way to stop a board used to
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
  `issue_runs` row. Liveness, dedupe and budget are asked there, once each —
  and the two a caller can be told *before* the write (liveness, and whether
  the runner can still host a session) are `enqueue_refusal`, so the two
  callers that must clear the way first — the promoter's move out of Todo,
  the block wake's stand-down — ask them before doing anything they cannot
  undo.
- **At most one unsettled run per issue**, enforced by a partial unique index,
  not by a check. **Which row that is has one home** — `runs::is_unsettled`,
  read off `settled_at` and never off `status`, because `idx_issue_runs_live`
  and `settle_run`'s own `WHERE` are what arbitrate the slot. Three doors ask
  it: a comment's delivery, the block wake's stand-down, and the unblock's
  hand-back. Spelled separately they agree only because `settle_run` happens
  to write both fields together, which is a coincidence, not a type.
- **A run row is written before anything is told about it**, and a failed
  dispatch costs a delay until the next boot sweep, never a lost run.
- **A finished card takes no runs** — `stages::is_finished` is the one
  definition, and a row already recorded on such a card is called off rather
  than left unsettled.
- **One home for "may the board hand this recorded row out now?"** —
  `runs::verdict`, asked by every hand-out door through
  `ProjectManager::card_for`: the boot sweep, the hold release, the dead-hold
  sweep and the unblock. `enqueue` is the door that *writes* the row and asks
  its own gates instead.
- **The sweep is metered.** A recorded row is handed back out at most
  `MAX_RUN_RESUMES` times, and a row nobody ever claimed waits at most
  `MAX_QUEUED_WAIT_HOURS` from the later of its own `created_at` and its
  card's `updated_at`; past either it is settled `Failed`, because nothing
  else on the board ever says the sweep gave up. The log line names which of
  the two bounds fired (`GaveUpOn`) — `resumes` alone cannot say, and an age
  give-up read as a resume give-up that somehow fired at zero.
  The count lives in `issue_runs.resumes` and not in a tally over timeline
  entries: a timeline append is explicitly allowed to fail without failing the
  thing it describes, so an events-derived counter undercounts on exactly the
  boot where the append failed — and the loop comes back.
- **Archived is read-only for a board's contents** — issues, comments, the
  team, a retry, and starting a run — enforced in `writable_project`, which
  every one of those writes starts with, the sweeps included (through
  `resume_project_runs`). Three writes do not ask, and each is deliberate:
  `set_project_archived`, or a board could never be restored; the operator's
  bookkeeping (`mark_issue_read`, `mark_project_read`, `cancel_run` — stopping
  work and noting it was seen are not additions to the board); and
  `record_event`, which describes work
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
