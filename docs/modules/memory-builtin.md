# memory-builtin — the memory that ships with the assistant

## Overview

The memory that **ships with the assistant** rather than being configured
into it: one markdown file per remembered
fact under a per-agent `memory/` directory, indexed by a `MEMORY.md` that
rides the system prompt, written and pruned by the model itself with the
ordinary `Read` / `Edit` / `Write` tools plus a small `MemoryDelete`, and
tidied by a scheduled **dream** pass that also rebalances what belongs in
memory versus what belongs in the profile identity files.

It is a cross-crate feature, not a crate. The pieces live where their kind of
code already lives:

- `crates/workspace/src/{paths,memory,prompt,identity}.rs` — addresses,
  auto-seeding, the empty-index template, the persona baseline commit.
- `crates/context/src/prompts/soul.rs` — the `<memory>` section and the rules
  that govern it.
- `crates/tools/src/builtin/{managed_repo,edit,write,memory_delete}.rs` — the
  audited-write tiers (identity + memory, shared by all three tools) and the
  delete verb.
- `crates/cron/src/scheduler.rs` — the dream job, seeded through the generic
  built-in-job path.
- `crates/agent/src/actor/router/cron.rs` — the dream fire's digest and skip.
- `crates/config/src/memory.rs` — `memory.builtin.*`.

**This is not [`memory.md`](memory.md).** That is the pluggable `Memory`
trait (`mem0` / `openviking` / `noop`), a single `Arc<dyn Memory>` slot with
`recall` / `on_turn_complete` hooks and an external store. This runs
alongside it and shares nothing: no trait impl, no slot, no config field. A
deployment can run both, either, or neither.

The axis in the name is deliberate. Both are stores of remembered things, and
the pluggable one could be file-backed too — what distinguishes this one is
that it is part of what the assistant *is*, which is also why it is the one
that is **on by default**.

The three hard constraints [`memory.md`](memory.md) carries forward are met
by shape rather than by policy: relevance is judged by the model reading an
index (never substring match), saves are single model-judged facts (never a
whole assistant output), and the index rides the seeded system prompt exactly
like the identity files (never a per-turn `Role::System` re-injection).

## Layout

Memory is partitioned **per agent**, and an agent's partition is wherever its
persona already lives:

```text
<persona>/                         # personas/<agent_id>/, or personas/project/<agent_id>/
  SOUL.md  IDENTITY.md  USER.md      # the agent's identity files
  skills/                            # the skills only it sees
  memory/
    MEMORY.md                        #   the index — one line per memory
    cat-name.md                      #   one fact per file
```

The built-in is just another persona directory (`personas/baybo/`), so its
memory lives beside its soul like everyone else's. Newly created project
agents use `project-<ULID>` ids and are grouped under `personas/project/`;
legacy unprefixed project personas remain flat and valid. There is no shared
memory tree — nowhere for one agent's writes to land in another's reach.

A custom agent's tree is materialised by `ensure_persona_layout` at creation;
the built-in is skipped there (it has no create step), so its index is seeded
alongside its identity files by `seed_default_identity_files` at setup.
Either way the `MEMORY.md` joins the persona baseline commit, so the first
real change reads as a change rather than as the file appearing from nowhere.
The index is also **auto-seeded on read**
(`baybo_workspace::load_memory_index`), so a deleted `MEMORY.md` is recreated
on the next assembly instead of leaving a half-formed prompt — the same
contract the identity files have.

`AgentProfileId::memory_dir` / `::memory_index_file` resolve the pair.

## What the model sees

`prompts::soul::assemble` emits, in order: the top hint, `<soul>`,
`<identity>`, `<shared_user_profile>`, `<user_notes>`, **`<memory path="…">`**
carrying `MEMORY.md` verbatim, **`MEMORY_HINT`**, then the background-work
and tag-handling hints. Declarative content first, operating rules last.

`PromptShape` separates card runs from ordinary conversations:

- `Chat` — every session that is not a card run, including every cron fire. It
  gets the Edit affordance, both user sections, and the general conversation
  memory rule.
- `Issue` — a card run. It uses the board memory rule and drops both the Edit
  affordance and `<shared_user_profile>` / `<user_notes>`: nobody is at a
  keyboard, and on the live board all seven agents' `USER.md` were byte-identical
  unmodified seed templates with the shared file blank — 806 bytes of empty form
  on every request, which an agent then reads as one more thing to fill in.
  Existing sessions retire old copies of those sections at their next
  compaction.

The generic memory rule names "the state of ongoing work" and defines `project`
as "ongoing work and where it stands". On a board that invites a stale copy of
the card: of 32 memories the live board's agents had written, **18 were
card-state snapshots** ("#12 approved, lead merge pending") with no writer to
keep them current. The `Issue` shape instead prohibits card-local state first,
then allows only reusable knowledge — environment behaviour, a board-wide rule,
a codebase trap, or an expensive-to-rediscover reference. The frontmatter
contract does not vary: both shapes still document all four types.

`PromptShape::for_trigger` is the only constructor. It reads the trigger's
canonical issue accessor: an issue run gets `Issue`; every other trigger gets
`Chat`, including a cron fire carrying a `project_id`.

Only the **index** is injected. A memory's body costs nothing until the model
decides an index line is worth a `Read` — which is what lets the tree grow
past what any prompt budget could hold. The hint teaches the shape:

- one fact per file, frontmatter `name` / `description` / `metadata.type`
  (`user | feedback | project | reference`), `[[links]]` between memories;
- update `MEMORY.md` in the same breath, because an unindexed memory is
  unfindable;
- check for an existing file before creating a duplicate; don't record what
  the identity sections already say, or what only matters in this
  conversation;
- delete what turns out wrong; keep the index skimmable.

The schema lives **only** in that prompt text and the dream prompt. There is
no frontmatter parser, no validation, no typed mirror — every reader and
writer of these files is an LLM, and a Rust-side schema would be a second
source of truth that can only drift.

**Subagents get no memory section.** `try_resolve_system_prompt` returns a
subagent's profile prompt before persona resolution runs, so the exclusion is
structural rather than a flag.

Lifecycle is the existing one: resolved at session seed and re-resolved at
each post-compaction reseed — plus the per-call freshness reconcile
(`reconcile_system_prompt`, see [context.md](context.md#the-system-prompts-lifecycle)),
which notices that `MEMORY.md` has moved and appends the new index at the tail.
So a memory written mid-session is in front of the model on the next LLM call,
and folds back into the system row itself at the next reseed (which also drops
the updates it makes obsolete). The index is the part most likely to change
twice in one session and also the largest, so it is the main reason each update
carries only the parts that moved rather than the whole prompt.

## Writing, and the audited tier

`crates/tools/src/builtin/managed_repo.rs` owns the workspace roots whose
writes are audited instead of approved, in three tiers:

| Tier | Roots | Approval | Allowlist | Size cap | Audit commit |
|---|---|---|---|---|---|
| Shared profile | `personas/USER.md` | bypassed for `Edit` | yes | 1 MiB | yes |
| Identity | `<persona>/{SOUL,IDENTITY,USER}.md` | bypassed for `Edit` | yes | 1 MiB | yes |
| Memory | `<persona>/memory/**` | bypassed | **no** | 1 MiB | yes |
| Scratch | `work/` | bypassed | no | no | no |

The per-agent tiers are **owned**: `<agent_id>` is the calling agent, and a
write into another agent's directory is refused — the same rule the identity
files already carried. The approval *declaration* has no call context, so
the bypass is decided on path shape and ownership is enforced at execute
time: a cross-agent write skips the gate and is then refused outright, which
writes nothing.

### When two agents write the shared profile at once

The dream pass fans out, so concurrent writers are routine rather than
exotic. Two things keep that honest, and neither is a merge:

- **Content: compare-and-swap.** `Edit` refuses a file this session has not
  `Read`, and refuses one whose `(mtime, size)` changed since that read. So
  the second writer does not clobber the first — it is told the file moved
  and to read it again, and its edit then applies to the current content. A
  blind lost update is not reachable; a *dropped* one is only as likely as
  the model declining to retry, which the rejection message asks it to do.
- **History: one commit at a time.** `git` holds `.git/index.lock` across
  `add` + `commit` and a loser exits rather than waiting, so parallel fires
  would have turned audit commits into `commit_warning`s — the change on
  disk, unrecorded. Audit commits therefore serialise on a process-global
  lock (`baybo_workspace::personas_git_lock`), shared with `personas/`'s
  other writer — the baseline commits — so those two cannot race each other
  either. There is one `personas/` repo, and the critical section is a few
  short subprocesses.

The **shared profile** (`personas/USER.md`) is the deliberate exception:
it belongs to no agent and every agent may read *and* write it. What one
agent learns about the person is worth the others knowing, so the stable
facts live in one place rather than being rediscovered per agent. That does
make it a write channel between agents — the one place the partition does
not hold — which is why it stays the *shared* file and each agent's working
notes stay in its own `USER.md`.

- **No filename allowlist** for memory: a memory tree is a freeform set of
  markdown files the agent names as it likes, unlike the identity dirs which
  are declarative slot stores.
- **A decoy filename in the wrong directory earns nothing** — every guard is
  positional first.
- **The size cap belongs to the verbs that touch bytes**, not to path
  resolution: `Edit` slurps the file so it refuses an oversized one, `Write`
  bounds the incoming body — and `MemoryDelete` applies no cap at all. A file
  that somehow crossed the cap must still be *removable*; capping the
  resolver instead would leave it unreadable, unwritable and undeletable,
  and if it were `MEMORY.md`, injected at that size into every prompt until
  someone opened a shell.
- **`Edit` and `Write`** both carry the memory treatment. `Write` caps the
  *incoming body* rather than the existing file (a fresh create has no
  existing file to measure), and recognises a byte-identical rewrite as a
  no-op instead of reporting the resulting empty `git commit` as a failure.
- **`Write` gets the identity tier's ownership check and audit commit, but
  not its approval bypass.** A soul is amended with `Edit`; a whole-file
  overwrite of one is the rare, deliberate act that should still meet the
  gate. The two halves are decided independently — `accessed_resources`
  grants the bypass, `audit_target` records the change — so a write the user
  approved still lands in the history, and one aimed at *another* agent's
  soul is refused outright rather than merely prompted for.
- A commit failure (detached HEAD, no `git`) never undoes the write — it
  surfaces as a `commit_warning` line.

### Membership has to be real, not lexical

`Path::starts_with` is purely lexical, so "is this path inside the agent's
own directory" is only as strong as what is checked around it. Three things
are refused, each because the memory tier's *lack* of a filename allowlist
makes location the entire guard:

- **`..`** — `absolutise` deliberately leaves it intact, so
  `<personas>/<id>/memory/../SOUL.md` passes a bare prefix test.
- **`.git`** — `personas/` *is* a git repo, so its `.git/` sits inside the
  tree it audits. Writable, that is an execution hole rather than an
  untidiness: `git` reads config from the repo it runs in, and
  `core.fsmonitor` or a `filter.*.clean` command runs an arbitrary program on
  the very next `git add` — which the audit commit itself performs. A deleted
  `.git/HEAD` would likewise turn every later audit into a warning line,
  quietly removing the accountability the approval bypass is premised on.
- **symlinks** — a link at the target name, or at any directory on the way to
  it, makes the string say one thing and the write land somewhere else, since
  `fs::write` follows links. `reject_symlinked_path` walks the path back to
  `personas/` and refuses any link on it.

The first two live in `baybo-workspace` beside `absolutise`, whose own
doc-comment warns that it leaves `..` intact for exactly this reason — and so
does the **shape recogniser**, `classify_persona_path`. Recognising
Recognising a persona memory path is the same knowledge as constructing it, and the
constructors (`persona_identity_file`, `persona_memory_dir`,
`shared_user_file`) are right there: a recogniser kept in another crate would
go on matching a layout that had moved, and it is the recogniser that grants
the approval bypass. `managed_repo` keeps the *policy* — which shapes are
audited, which need ownership, which skip the gate.

### `MemoryDelete`

A dedicated tool that removes one file from the calling agent's own memory
tree (anything else is refused outright) and audit-commits the removal.

It prompts for nothing. A memory file is audited rather than approved, and
any other path is refused before it touches the disk — so a gate could only
ask the user to sanction a deletion that cannot happen either way, and asking
anyway would teach them that approving this tool is harmless. The refusal is
in the tool result and the trace.

It does **not** rewrite `MEMORY.md`. The index is markdown the model authors
in whatever shape it likes, and a tool editing lines by pattern would
eventually mangle one. But a dangling entry is the one thing the index cannot
tolerate — it rides every prompt and would send the model to `Read` a path
that is gone — so the tool checks whether the index still names the deleted
file and says so in its output. Removing the line stays the model's job; not
noticing is no longer possible.

It exists because Bash `rm` gating is **permission-mode-dependent**: under
the default `auto` mode an LLM risk judge can wave a scoped `rm` through
unprompted, under `manual` it routes to a channel approval an unattended
dream pass will never get answered, and `free` skips destructive checks
entirely. `MemoryDelete` makes forgetting deterministic, root-scoped and
audited whatever the bash mode is.

(The repo's never-delete rule governs session rows and transcripts. A memory
file is agent-authored content whose history is one `git revert` away;
deleting one is the intended maintenance path.)

## The dream pass

A **runtime-owned recurring cron job** (`BuiltinCronJob::Dream`), seeded at
boot through the same `CronScheduler` the model's own jobs use. The job is
one instance of a general shape — `BuiltinCronJob` names the runtime's jobs,
`CronScheduler::ensure_builtin_job` seeds and reconciles any of them, and
`BuiltinFireContext` carries whatever fire-time material one needs. Adding a
second system job is a variant, its title and prompt, a boot call, and (only
if it needs fire-time material) an arm in the router's built-in dispatch. Its
fire is an ordinary recurring-cron session: full agent loop, trace, billing,
a sidebar cron group that doubles as the browsable dream journal, and
`report_nothing` when a pass finds nothing worth surfacing.

### One pass, one conversation per agent

Each agent keeps its own memory and its own identity files, and the write
tier refuses one agent's writes into another's — so a single fire could only
ever tend whichever agent it happened to run as. The pass therefore fans
out: `fan_out_dream` groups the window's conversations by owning agent and
opens one fire per agent, each **bound to that agent**, seeing only its own
conversations. They share one job, one
schedule and one cron group, so what the user sees is still a single pass.

An agent with no activity gets no fire; when no agent has any, nothing is
minted at all. Only `baybo`-framework agents dream — an external framework
runs its own CLI with its own tools and never sees the `<memory>` injection,
so firing the pass at one would spend a turn asking a stranger to tidy a
room it cannot see. An agent whose profile row is gone is skipped too: the
write tier is keyed on the id, so nothing would read what the pass wrote.

### Seeding is a seed, not an assertion

`CronScheduler::ensure_dream_job` reconciles at every boot:

| Row | `memory.builtin.enabled` | Result |
|---|---|---|
| absent | on | created with the configured schedule |
| absent | off | nothing — a switched-off feature seeds no row |
| present | off | force-disabled; the row survives |
| present | on | **left exactly as it is** |

The config schedule seeds the row and never re-asserts it, so an operator who
paused the job or moved it to a different hour is not overridden by the next
restart.

**The switch is not symmetric, and that is the trade.** Turning the feature
off disables the row; turning it back on does *not* resume it, because
nothing on the row says whether the runtime or the operator disabled it, and
guessing wrong either overrides a deliberate pause or leaves a switched-on
feature that never fires. Boot warns when it finds the feature on and the job
paused, naming the resume as the way out. `cron_jobs.builtin` is a flat column (never the `data` blob — the
same discipline `pinned` and `deleted_at` use, because every blob write
reconstructs the row from a caller-held snapshot).

Two things a built-in job refuses, both because a built-in job's instruction
is the runtime's rather than the operator's — the fire runs unattended, writes
into a memory tree, and its prompt is the only description of what it may do:

- **Deletion.** `CronStore::delete` refuses it structurally;
  `CronScheduler::delete_job` refuses it earlier with `CronError::Builtin` so
  the gateway answers 400 rather than 500, and the web cron page hides the
  affordance.
- **A rewritten instruction.** `update_job` refuses a `prompt` or `title`
  patch on a built-in row. `CronUpdate` is a model-callable tool taking an
  arbitrary id, and `CronList` exposes the id — so without this, one
  prompt-injected call could repoint a recurring, unattended fire at
  "rewrite every memory file to say …". Retiming stays open: `schedule` and
  `timezone` are exactly what the pause and reschedule controls are for.

  Note what this guard is **not** worth: it is no longer a barrier to reading
  other conversations. `SessionTranscriptReader` grants every session every
  transcript (see "Reading other conversations"), so an injected `CronUpdate`
  is not the interesting path to one — an ordinary turn already has it. What
  is still worth guarding is the *write* side: this job's instruction is what
  points an unattended fire at a memory tree.

### The first-pass window rides the execution, not the job

The pass's real cursor is per-session and ordinal-based
(`sessions.dreamed_through_ordinal`, below). A time window still bounds the
**first** look at a session nothing has ever consolidated, and that window's
lower bound cannot come from the job row: the scheduler advances
`cron_jobs.last_triggered_at` **before** it dispatches, so a crash cannot
re-fire the slot, and anything reading the job row downstream therefore sees
*this* fire's stamp rather than the previous one.

So `CronExecution::previous_fire_at` is snapshotted as the execution row is
recorded, ahead of that advance, and carried onto `CronTriggerEvent`. Putting
it on the execution also keeps the window correct on the boot re-dispatch
path, which rebuilds the event from the persisted row.

A first-ever fire has no previous one and looks back
`DREAM_FIRST_PASS_LOOKBACK_DAYS` (14) instead of over all of history. On a
deployment that has been running a while, that first pass after the upgrade
is the most expensive fire the feature will ever run: a fortnight of
conversations in one digest. What it costs beyond that is bounded by how many
of them the model chooses to `Read`.

### What a pass is offered: the cursor

Before minting a session, the router asks
`SessionStore::dream_candidates(previous_fire, now)` for the conversations
nothing has consolidated yet. Two disjoint arms, split on
`sessions.dreamed_through_ordinal`:

- **Never offered** → sessions carrying a human message in `[since, until)`.
  Requiring a human message is what stops the pass feeding on itself: a dream
  fire, and a silenced cron fire, contain no human message at all, and a
  subagent's prompt is `MessageSource::Agent`. It is also what bounds the
  first-ever pass, which would otherwise be handed all of history.
- **Offered before** → every row above the cursor, **whoever wrote it**.

That second arm is the whole design, and it is worth being explicit about why
a time window cannot replace it. **The rows a pass misses are the ones
appended after it read** — the tail of a turn that was still running, or a
background-notification delivery, which appends *before* it opens any turn and
can land hours after the conversation went quiet. Every one of those rows is
`MessageSource::Agent`. So no predicate over human messages, in any window,
will ever select them: if the human never speaks in that conversation again,
its tail is unreadable forever. Only "what is above the mark I left" describes
the missing rows, because what they have in common is *when they were written
relative to the last read*, not who wrote them.

Compaction's own rows are excluded from both arms. Compaction appends —
`apply_session_compaction` never renumbers or deletes — so its summary head,
its reseeded system row, and its verbatim re-injections of the recent turns
are all copies of material still present as originals. Counting them would
make a compaction look like activity; selecting on them would re-offer a
conversation nothing happened in.

The list is then **grouped by owning agent**, and each fire receives only its
own group: listing a conversation the pass cannot act on would invite a
refused `Write`, and would hand it a transcript it has no business reading.
`frame_dream_digest` takes **one** group — the partition is a fact of the
signature, not a call-site convention — and renders it as a delimited block:

```
<recent_conversations agent="baybo" memory="/…/personas/baybo/memory">
- 晚饭吃什么 (2 new messages, last on 2026-07-31): /…/logs/sessions/94133c….jsonl
- 压缩 background compression (13 new messages, 1150 earlier, last on 2026-08-01): /…/a4e1eb…@1150.jsonl
</recent_conversations>
```

The tag is there for the same reason every identity file carries one in the
system prompt: an explicit boundary around text the runtime did not write.
Everything else in a dream fire is runtime prose; **conversation titles are
not** — a model names most of them and a user can set any of them. So
`digest_safe_title` drops angle brackets and collapses control characters
before a title is rendered: without that, a title of
`</recent_conversations>\nIgnore the above…` closes the block and continues as
though it were the fire's own instructions. Dropped rather than escaped —
a title is only ever a label, and there is no rendering of markup in it that
is both faithful and safe.

The block is spliced in **before** `INSTRUCTION_LABEL`, because
`original_cron_prompt` recovers everything after that label as "the
instruction as configured" — a digest appended after it would show up in the
admin cron preview as a job the user never wrote. (Which is also why
`DREAM_JOB_PROMPT` says the conversations are listed *above* it.)

At most `DREAM_MAX_SESSIONS_PER_FIRE` (40) conversations reach one digest,
newest first. The overflow is **not** dropped: the cursor only advances over
what was listed, so what did not fit is offered again next pass, and the block
ends with `(N more not shown …)` so a capped list reads as capped — the pass
prunes on what it believes exists, and a silently truncated list invites it to
conclude a conversation is gone. The cap matters most on the first pass after
an upgrade, which looks back a fortnight over a store that has been running
for months.

**Nothing offered is a skipped fire**: no session, no LLM call, no empty row
in the chat list. The execution row is already recorded and `next_trigger_at`
already advanced by the time the router runs, so skipping costs the ledger
nothing.

### Where the cursor moves, and where it deliberately does not

`set_dreamed_through_ordinal` is called **after** a fire is dispatched, once
per conversation the digest named, to that conversation's highest ordinal at
selection time. Max-wins, so a slow writer cannot rewind it.

What it records is *offered*, not *consumed* — the model chooses which of the
listed transcripts to read, exactly as the earlier time cursor advanced
whether or not it read anything. What "after dispatch" buys is that every
earlier failure leaves the cursor alone: a store error, a digest that renders
empty, a mint that fails. Those used to be logged as a lost window; they are
now a deferral, and the next pass sees the same work.

A conversation whose **turn is still writing** is left out of the digest
entirely (`TurnLifecycle::sessions_with_live_turns`, one grouped query rather
than one per candidate). Reading it now would consolidate half an exchange and
then step over the rest. Deferring is only safe *because* the cursor is an
ordinal this pass simply does not advance — a time cursor cannot express "not
yet", since a deferred message's `created_at` does not move and the next
window starts after it. The live-turn query failing defers everything rather
than reading anyway: an unknown turn state plus a cursor that advances is
exactly how rows go missing, and a deferral costs one skipped pass.

Deferral has no timeout, deliberately — waiting is the safe direction — but a
turn that never settles would then skip its conversation for the life of the
process. A `warn!` names the deferred sessions once per pass, because a stuck
turn is an operator problem and this is the only place that sees it as one.
(`with_turn` is what keeps such rows rare: every exit past `start()` settles
the row, so only a crash leaves one behind, and boot recovery closes those.)

The two structural skips — an agent on an external framework, an agent whose
profile is gone — do **not** advance anything either. Their conversations are
re-selected every pass and dropped again at `dream_framework`, which costs a
query row and no LLM call; advancing instead would silently lose their history
if that agent ever became a `baybo` one.

### Reading only what is new: the `@<ordinal>` anchor

`DreamCandidate::read_from_ordinal` — the oldest unconsolidated row — becomes
the digest's path via
`WorkspacePaths::session_log_file_from(id, read_from_ordinal)`:
`<id>@<ordinal>.jsonl`. `SessionTranscriptReader` recognises that form and
serves from that message on, so a conversation running for months costs a pass
only what is new to it. Ordinal 0 collapses back to the plain path, so there
is only ever one spelling of "from the beginning".

Without it a long conversation is re-read from row 0 every pass — and worse
than merely repetitive: `Read` defaults to the first 800 lines, so a
months-old transcript hands back its opening pages, the part already
consolidated, while the new activity sits past the end of the page.

Three details keep it honest:

- **Headers carry the stored ordinal, not the slice index.** A render that
  restarted at `[0]` would read as a whole conversation and misplace every
  reference into it.
- **The digest says how much it is skipping** (`4 new messages, 312 earlier`),
  and the heading tells the pass it may drop the `@<n>` when the new messages
  only make sense in context. Nothing is hidden — it is just not paid for by
  default.
- **The suffix is validated by round-trip**, exactly like the id half: `@007`
  and `@-3` parse but are not what the composer emits, so they are refused
  rather than silently rounded. `@` is safe as the separator because
  `sanitize_session_id` rewrites it, so it cannot occur inside the id.

The bound is pushed into SQL (`load_session_messages_with_supersede_since`),
and `(session_id, ordinal)` is the primary key, so reading the tail of a
months-old conversation is an index range scan costing what that tail costs.

### Reading other conversations

`SessionTranscriptReader` serves a session its own transcript, and any other
session's — **there is no identity check**. This was own-session-only, then
briefly same-person-only, and neither survived contact with real data: keying
on the caller's identity denied the pass most of the conversations it exists
to consolidate. See [`docs/todo/user-identity.md`](../todo/user-identity.md).

That is what lets the pass read those conversations with no capability
plumbing at all — no per-turn allowlist threaded from the router through the
loop to the tool context, which is what this used to take. Three details keep
it honest:

- The resolver serves the **matched** session's transcript, not the caller's.
  Serving `access.session_id` for another path would hand the fire its own
  empty transcript for every conversation it asked about — wrong content, no
  error.
- The id is not parsed out of the path. `sanitize_session_id` is lossy, so a
  candidate is only accepted when it **round-trips** through the same path
  composer; an id that would need sanitising is simply unaddressable this
  way, which costs nothing because every id this system mints is already
  safe.
- **Compaction's rows are dropped, not rendered.** This read exists to serve
  the pre-compaction detail, and the originals are all still there — so the
  copies add nothing, while rendering them would show the recent exchange
  twice and present the summary head as something the human said (it rides as
  `Role::User` with `MessageSource::Agent`, which the render labels `user`).
### What a pass does

Prompt-defined, in `DREAM_JOB_PROMPT`: record what is worth carrying forward,
consolidate duplicates, prune what is wrong or superseded, rewrite the index
— and **rebalance the identity ⇄ memory boundary in both directions**.
Identity files ride every prompt in full while memory bodies cost nothing
until read, so the pass promotes what every conversation needs (a hardened
fact about the human into its `USER.md`, a real shift in how it works into
its `SOUL.md`) and demotes the long tail out of those files into memory,
leaving only an index line in context. Those files stay lean; memory carries
the detail.

The pass only ever **reads** transcripts.

#### The pass has two inputs, and it can delete

Both are corrections to an earlier version that could restructure the
identity files but never actually shrink them.

**It is shown the budget.** The router prices the agent's assembled prompt
per file (`AssembledPrompt::budget`) and splices the figures into the fire
beside the digest. Without them "keep them lean" is an adjective with nothing
behind it, and an observed pass trimmed until the diff felt substantial and
stopped — well short of the target, because it had no target. The count is an
estimate: the encoding is chosen at the fire, not by the model the fire runs
on, which is the right trade for a number whose only job is to steer how hard
the pass trims.

**It may delete, not only demote.** The prune step covers the four files that
ride the prompt, not just the memory tree, and names the criterion: a line
recording what the human asked *once*, carrying no reusable instruction, is
cut rather than filed. This is the load-bearing half. Demoting moves a token
into an index line; only deleting returns one — so a pass whose sole shrink
verb was "demote into memory" conserved the cost it was sent to reduce, and
the growth showed up in the index instead.

The rebalance step names all four files. An earlier version named only
`SOUL.md` and the agent's own `USER.md`, which left the **shared** profile —
routinely the largest, and the one every agent pays for — under a step whose
only verb was "update when you have learned something durable": append
semantics, with no way to shrink.

## Config

```jsonc
"memory": {
  // …the pluggable-backend knobs (enabled / provider / llm / extra)…
  "builtin": {
    "enabled": true,                          // default ON
    "dream": { "schedule": "0 4 * * SUN,WED" } // ≈ every 3.5 days, job-local time
  }
}
```

Orthogonal to `memory.enabled` / `memory.provider`, which govern the
pluggable backend only. Disabled ⇒ no `<memory>` section, no `MemoryDelete` tool (a verb for tidying
a directory the prompt never mentions would only confuse), and a
force-disabled dream job. The tree itself is still *materialised* with the
rest of the persona layout — the flag governs reading and writing, not the
skeleton, which is what keeps switching it back on a no-op. Files
are never deleted by the runtime regardless of config. Not hot-reloadable,
like the rest of memory config.

**Named cron days, not numbers.** The pinned `cron` crate rejects a
day-of-week `0` and counts Sunday as `1`, so every numeric spelling of
"Sunday and Wednesday" is either a parse error or the wrong pair of days.
`DEFAULT_DREAM_SCHEDULE` uses `SUN,WED`.

## Constraints

- Feature subsystem, not a crate; domain types where their kind lives.
- Ownership-keyed guards, matching the identity files: an agent writes its
  own tree, and the tool layer — not prompt discipline — is what enforces it.
  The shared `personas/USER.md` is the one deliberate exception.
- The dream pass fans out to one fire per active agent, each bound to that
  agent and tending only its own memory.
- Nothing in the memory tree is ever deleted by the runtime; only the model
  deletes, and only through an audited commit.
- No new CLI surface (`baybo memory` stays trait-only).

## Known limitations

- **A relayed stranger's conversation is consolidated like the operator's
  own.** Neither `dream_candidates` nor `SessionTranscriptReader` filters on
  `ChannelKind::Multiplexed`, so on a deployment with a Telegram / WeChat /
  Discord sidecar the people that bot relays are dream candidates: their
  transcripts get read, distilled into the agent's memory tree, and — step 5
  of `DREAM_JOB_PROMPT` invites exactly this — durable facts about "your
  human" can be promoted into the shared `personas/USER.md`, which every
  agent loads in full on every turn.

  This is a **decision, not an oversight**, and it rests on an assumption
  worth stating out loud rather than leaving implied by "the deployment
  serves one person": today's deployments have one human, and identity is not
  a boundary this system can currently draw
  ([`docs/todo/user-identity.md`](../todo/user-identity.md)). Note that
  `user.id` would be the wrong fix — it splits one human across several ids —
  whereas `ChannelKind::is_multiplexed()` is a compile-time property of the
  channel implementation and would be the right one. **Filter on it before
  running this feature on a deployment with a sidecar channel.**

- **Any session can read any transcript.** `SessionTranscriptReader` enforces
  nothing beyond the path round-trip (see "Reading other conversations"), so
  this is not scoped to the dream pass: an ordinary turn, a subagent, or a
  relayed conversation can `Read` any session's full transcript given its id.
  Ids are unguessable (`SessionId::new()`), so this needs a leaked id rather
  than enumeration — but the dream digest prints them, and so do traces and
  logs.

## Deferred

- **Dreaming for an external-framework agent.** `claude` / `codex` agents
  never see the `<memory>` injection, so the pass skips them. Giving them a
  memory means teaching the external leg to read and write the tree, not
  scheduling.
- **Manual "dream now."** `CronScheduler::trigger_now` implements manual
  firing but has no caller (no CLI, REST or tool), and does not stamp
  `last_triggered_at` for a recurring job — exposing it needs both a surface
  and cursor-advance parity, or the next scheduled pass would re-read the
  same window.
- **Memory search.** The index is read linearly by the model. A tree large
  enough to need `Grep` can already be grepped; a dedicated tool is only
  worth it if the index itself stops fitting.
- **Poisoning defence.** Channel content can talk the model into saving an
  attacker-shaped "fact" that then rides every later prompt. Mitigated today
  by the audit trail (`git revert`), the dream pass's consolidation, and the
  existing input-sanitization layer — and, inside the digest, by
  `digest_safe_title`, which stops a conversation title closing the block and
  continuing as instructions. Worth revisiting if external/multi-user
  channels widen; see the first entry under "Known limitations", which is the
  same exposure from the other end.

## Collaboration

| Module | Role |
|---|---|
| `workspace` | `PERSONA_MEMORY_DIR` / `MEMORY_INDEX_FILE`, the path helpers, `load_memory_index` auto-seeding, `ensure_persona_layout` (dir + index + baseline commit) |
| `model` | `AgentProfileId::{memory_dir, memory_index_file}`, `BuiltinCronJob`, `CronJob::builtin`, `CronExecution::previous_fire_at` |
| `context` | the `<memory>` section + `MEMORY_HINT`; `frame_cron_prompt_with_context` + `frame_dream_digest` |
| `tools` | `managed_repo` (roots, ownership, guards, audit commits — shared by `Edit`, `Write` and `MemoryDelete`), the memory tier, `MemoryDelete` |
| `cron` | `BuiltinJobSpec` + `ensure_builtin_job`, `CronError::Builtin`, `previous_fire_at` on the trigger event |
| `store` / `storage` | `UserActiveSession` + `sessions_with_user_messages_since` (+ its index), `cron_jobs.builtin` |
| `agent` | the dream digest (own-agent only), the empty-window skip, `BuiltinFireContext`, `SessionTranscriptReader` |
| `config` | `BuiltinMemoryConfig` / `DreamConfig` under `memory.builtin` |
| `gateway` / `web` | `CronJob.builtin` on the wire; the cron page hides delete for a built-in job |
