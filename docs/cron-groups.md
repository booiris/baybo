# Cron groups — collapsing a job's fires into one chat-list row

A recurring cron job mints a **brand-new session on every fire** (`agent/src/actor/router/cron.rs`
`mint_fire_session`), and a recurring fire's session *is* a first-class conversation — listed,
replyable, pushable. A `*/30 * * * *` job therefore opens **48 conversations a day**, and they land
flat in the chat list next to real ones.

A **cron group** collapses all of one job's fires into a single list row. Tapping it opens a screen
holding that job's history.

> **It is a group, not a folder.** It is *not* a `session_folders` row, it is not in the user's
> folder tree, and it is not `sessions.folder_id`. Calling it a "cron folder" is how you get talked
> into building the row — see [Why nothing is stored](#why-nothing-is-stored).

## The model: nothing is stored

A cron fire session already knows which job it belongs to. The grouping is **read** from it:

```rust
TriggerSource::Cron { cron_job_id, origin_session_id, conversation, job_title }
```

Two fields carry the feature, and neither needs a schema migration:

| Where | Field | Why |
|---|---|---|
| `TriggerSource::Cron` (`model/src/session.rs`) | `job_title: Option<String>` | Snapshotted at mint. **Written once, never updated** — so it cannot go stale. Lives in the `sessions.data` JSON blob, so `#[serde(default)]` covers every historical row. |
| `ChatSessionSummary` (`gateway/src/api/admin/chat.rs`) | `cron_job_id`, `cron_job_title` | Both read straight off `session.trigger`. Populated only for `is_cron_conversation()` rows. |

Clients group the list by `cron_job_id` and label each group with `cron_job_title`.

### Naming a group

The gateway resolves the label in the chat-list handler, from a single `CronStore::list_all()`
(cron jobs number in the handful; the endpoint already fans out a per-row preview and unread scan,
so this is cheaper than what is already there):

```
group_name(job_id) = cron_jobs[job_id].title            // live: a job rename syncs to every client
                  ?? newest_member.trigger.job_title     // tombstone: the name it had when the job died
```

The live lookup is what makes a rename propagate **with no hooks and no rewrite of any session**.
The snapshot exists for exactly one question — *"the job is gone; what was this history called?"* —
which is why it is written once and never touched again.

A rename is a real operation (`CronUpdate`, `PATCH /v1/cron/{id}`), and group naming follows from the
live lookup with no work of its own. It also makes the two sources of the name observable at once: a
job renamed mid-life has fires snapshotted under **both** names, so the tombstone branch must take
the name off the **newest** member, as written above. A client that folds the members in the other
direction renders the name the job was born with, and disagrees with every other client.

A fire session's own title (`{job title} · {M/d}`) is **not** rewritten on a rename. The group takes
the new name; the historical rows keep the name they were fired under. That is history, not staleness.

**Unnameable groups.** A fire whose job is deleted *and* which predates the `job_title` snapshot has
no name available from either source. Those rows **stay flat** (ungrouped). The population is
self-limiting and shrinks to zero.

## Rendering rules (shared by iOS and web)

**Group membership**: rows where `is_cron_conversation()`, keyed by `cron_job_id`.

**Every row appears exactly once.** A fire renders *inside* its group iff
`!archived && !hidden && !pinned`:

- **Pinned** → escapes to the main list's pinned block. This matches web, where `s.pinned`
  short-circuits before any folder grouping (`SessionSidebar.tsx`).
- **Archived** → escapes to the archived view.
- **Hidden** (the soft delete) → gone.
- **`folder_id` is ignored** for a cron conversation. Web hides "Move to folder" on cron rows so a
  fire cannot be in a group and a user folder at once.

**The group's aggregates count only the rows drawn inside it** — preview, timestamp, unread sum.
Counting an escaped (pinned) member would double-count it: it is already its own row in the pinned
block, and the badge would disagree with what you see when you open the group.

**An empty group cannot be represented.** A group exists iff it has at least one visible member, so
there is no "hide empty groups" rule to write, and no ghost group can exist for a job on another
channel — the session list is already channel-scoped (`list_by_channel`), so a `telegram` job's
fires simply are not in your list, and neither is its group.

**History groups on ship day.** The grouping key is already on every historical fire session, so a
list that is already flooded collapses the moment this ships. There is nothing to backfill.

## Pinning a group

A group can be **pinned** to the top of the chat list. It is the group's only mutation, and it does
**not** give the group a row — the bit goes on the **cron job**, which is the only object whose
identity and lifetime match the group.

```
PUT /v1/cron/{id}/pin   { "pinned": true }      →  cron_jobs.pinned
GET /v1/chat/sessions   →  ChatSessionSummary.cron_group_pinned   (on every fire of the job)
```

**Why it needs a pin at all.** A group already sorts on its newest visible member — "48 fires a day
become one row moving" — so a job that fires often is *already* near the top and a pin buys it
nothing. The pin exists for the **low-frequency** job: a weekly digest sinks under ordinary chats
between fires, and that is the only case it serves. Do not sell it as more than that.

**Storage is `sessions.pinned`'s shape, verbatim, and it must stay that way.** `cron_jobs.pinned` is
a **flat column** written only by the targeted `CronStore::set_pinned`; `CronJob::pinned` is
`#[serde(skip)]` so it never enters the `data` blob, and `CronStore::save` never writes the column.
This is not ceremony:

> Every blob write reconstructs the whole row from a snapshot the caller holds — `record_fire` does
> it on **every fire** from the job read before the fire, and `save_if_unchanged` on an edit. A pin
> written into that blob (or added to a write's `SET` list) is a snapshot taken *before* the user
> pinned, so it is reverted by the job's next tick, minutes after it is set. This is the same
> flat-column discipline `cron_jobs.deleted_at` uses, for the same reason.
> `sqlite::cron::tests::a_fire_and_a_save_cannot_unpin_the_group` pins it.

**The read is free.** The chat-list handler already loads every job to resolve group *names*
(`live_cron_job_meta`), so the pin rides that same lookup with no extra query. Note the trap it
already contained: that map used to **drop untitled jobs**, which was harmless when it only carried
the title — and would silently unpin the group of any job with no title. The emptiness check now
lives in `cron_group_label`, not in the map.

**Two consequences, both accepted, neither an accident:**

- **The pin goes dormant when the job is deleted, and comes back on restore.** `CronStore::delete` is
  a recycle bin — it stamps `deleted_at`, leaving the `pinned` column untouched. A deleted job drops
  out of `list_all_jobs` (every listing filters `deleted_at IS NULL`), so its group renders with the
  tombstone name and reads **unpinned** while in the bin — the same observable state a hard delete
  would have given. `restore` clears `deleted_at` and the pin is simply there again. Nothing clears
  it on delete; a group's pin follows the job's lifetime, dormant in between.
- **The group pin and a fire's own pin are different things, and both hold.** Pinning one *fire*
  still escapes it from the group (above); pinning the *group* keeps the whole recurring stream at
  the top. A pinned fire inside a pinned group renders once in the pinned block and the group renders
  once as its own pinned row — nothing is drawn twice.

**Scoping.** The pin route 404s a job whose `channel` is not the caller's, exactly as
`load_scoped_chat_session` does for a session, so the `http` (web) and `device` (iOS) universes stay
disjoint. `list_cron` (an unfiltered operator surface) was no precedent to copy. Note the asymmetry
this leaves: the pin route is channel-scoped, but the sibling cron mutations that landed alongside it
(`update` / `pause` / `resume` / `restore`) are **not** — a follow-up should decide whether they want
the same scoping.

**No `SessionPatch`.** No session row changed, so there is nothing to patch; the gateway broadcasts
the session-less `Frame::Gap` (list-stale, `broadcast_list_stale`) and every client re-derives the
group's block on its next list pull.

## iOS

The chat list stays a flat `List` — `.plain` style, the in-content pinned tint, the hand-rolled
`contentMargins(.top, 58)` pull-to-refresh, and per-row `.swipeActions` are all untouched. It just
learns a second row type.

**The group row** looks like a chat row (so it costs no new visual language) with a leading
monochrome **clock glyph** — the user's mental model is *"my morning task"*, not *"a folder"*:

- bold title = the group name; second line = newest visible member's preview
- timestamp = newest visible member's `lastActive`; badge = **sum** of visible members' unread
- it **sorts by its newest visible member's `lastActive`, in the same sort as chat rows** — so when
  the job fires, the group floats up. 48 fires become **one row moving**.
- one swipe action: **mark all read** (see below). No pin/archive/delete — those are affordances of
  an object, and a group is a view.
- tapping **pushes a screen** (the same shape as the archived screen) listing that job's fires,
  drawn with the existing `SessionRowView` and its swipe actions.

> **Do not use `Section(isExpanded:)` or `DisclosureGroup`.** The disclosure chevron only renders
> under `.listStyle(.sidebar)`, and leaving `.plain` re-introduces system section insets that fight
> the in-content pinned tint and the custom pull-to-refresh. `DisclosureGroup` nests rows inside one
> cell, which kills per-row `.swipeActions` outright.

### Mark all read

`PUT /v1/chat/sessions/{id}/read` is per-session and takes an ordinal the list client does not have.
Marking a 48-fire group read one call at a time is 48 round-trips, which is worse over the relay
tunnel. So there is a batch endpoint — deliberately **generic**, not cron-shaped:

```
POST /v1/chat/sessions/read   { "session_ids": [...] }   → 204
```

For each id the gateway reads `latest_session_ordinal` and advances the read cursor to it (max-wins,
so a racing per-session mark cannot regress it). The client sends the **visible** members' ids;
escaped (pinned/archived) rows clear themselves where they live.

Without this the feature is cosmetic: the noise is tucked into a drawer whose red badge still reads
`48` and cannot be cleared.

## Web

The sidebar renders **two kinds of group**: real folders (draggable, deletable — unchanged) and cron
groups (derived, immutable, not a drop target). That asymmetry is honest — one is user-organised, the
other is machine-generated — and pretending they are the same thing is where every bug in the
rejected design came from.

No folder-CRUD guards, no greyed-out menu items, no new error surface. (Web's folder CRUD is
fire-and-converge with `console.warn`-only error handling, so a server-side `403` on a cron folder
would have shown up as *"I clicked Delete and nothing happened, with no message"*. A derived group
produces no 403 because there is nothing to refuse.)

## Why nothing is stored

`docs/modules/cron.md` used to recommend the opposite: *"auto-file each job's fires into a per-job
chat folder (`sessions.folder_id` and the folder tree already exist)"*. That paragraph is now wrong,
and it is worth recording why, because the folder-row design is the obvious one and someone will
propose it again.

The group supports **almost no mutations** — it cannot be deleted, renamed through the folder API,
reparented, have chats moved in or out, or host a new chat. It can be **pinned**, and that is the
only one (see [Pinning a group](#pinning-a-group) — the bit goes on the *job*, not on a group row, so
the argument below is untouched). **A thing with no state of its own does not need to be a row in a
mutable table.** Making it one means storing the same fact twice and then inventing machinery to keep
the copies in sync. An adversarial review of that design found these, all from that one root cause:

- **Every write to a cron job's row is a conditional one** (`storage/src/sqlite/cron.rs`): an in-place
  change CASes on the row it read, and a fire's write-back stamps its own four fields and refuses a row
  whose slot moved under it. A `folder_id` living on that row makes the router a **third** writer, which
  has to join that protocol or lose: a link written on a snapshot the fire has already moved is dropped
  silently, and the next fire — finding no link — mints a **second** folder. The first one holds a fire
  (so it is not empty), is named identically, and — with `delete_folder` refusing cron folders — **no API
  can ever remove it**.
- **A job deleted mid-first-fire** leaves the same immortal orphan, with no race at all: the delete
  hook reads `folder_id`, finds `None`, flips nothing — and the fire (whose event is snapshotted off
  the execution row) goes on to create the folder anyway.
- **`CronDelete` has two entry points** (`cron/src/tools.rs`, `gateway/src/api/admin/cron.rs`), so any
  hook that must run on job deletion has two places to be forgotten.
- **Folders are global; sessions are channel-scoped.** `session_folders` has no channel and no user
  column, so a `telegram` job's folder is a permanently empty header in *both* web and iOS.
- **`broadcast_folders` lives in the gateway** and `crates/agent` must not depend on it — so a folder
  created by the cron router would be broadcast to nobody, and would need a whole new sink.
- **`validate_folder_name` rejects names over 60 chars.** A cron `title` is model-authored and
  unbounded, so a long one makes folder creation **400 on every fire, forever** — and truncating it
  with `String::truncate` (which counts *bytes*) panics the router task on a Chinese title.

All of it becomes **unreachable**, not handled, when nothing is created: no write-back, no second
writer, no flag to keep in sync, no delete hook, no broadcast, no name validation, no migration.

The one thing the folder-row design bought — living in the user's folder tree, renameable and
sortable alongside their own folders — is a capability we explicitly ruled out.

## Not doing

- **Batch archive.** Mark-all-read clears the badge; *"never show me this job again"* is still
  "delete the job".
- **Showing the job's state on the group row.** A group whose job is paused or in the recycle bin is
  pixel-identical to a live one — the row carries `cron_job_id` and a title, not a status — so the
  only signal that a job stopped firing is that its group stopped growing. Closing that means putting
  the job's state on `ChatSessionSummary`, which is a wire change; it is not free, and it is not this.
- **Per-user scoping.** `session_folders` has no `user_id`, the session list scopes by channel only,
  and `CronList` calls `list_all_jobs()` unfiltered. A pre-existing gap; cron groups inherit the
  session list's visibility exactly and neither widen nor fix it.
