# `user.id` does not identify a user

> **Status:** open. Nothing depends on `user.id` for correctness right now —
> the dream pass was the last surface that tried, and it stopped (see
> [Where this came up](#where-this-came-up)). Fix this before anything else
> partitions on identity.

## The finding

`Session.user.id` is the id of **the code path that minted the session**, not
of the human who spoke. One person accumulates several ids, all live at once,
none of them wrong from the minting site's point of view. On a store that has
only ever served one human, the majority of sessions sit under an id that
human has never typed under.

### Where the ids come from

- **`owner`** — `crates/gateway/src/api/admin/chat.rs` (`chat_user`) and
  `crates/gateway/src/channel/route.rs` deliberately collapse every
  `owner`-channel sender to `auth::OWNER_USER_ID`. This is the current, correct
  behaviour: web and device are one identity sharing one memory and cost
  namespace.
- **`device-<pubkey-hex>`** — the paired phone's device-protocol public key,
  from before that collapse existed. Still being *written today*, because
  `cron_jobs.user_id` froze whoever created the job and every fire re-stamps
  it: most cron jobs in a long-lived store carry a device id, and so do their
  fires.
- **`ios-<pubkey-hex>`** — the same phone under the retired `ios` channel and
  the retired `ios-` id prefix (the rename that also left zombie push
  bindings).
- **`web-operator`** — `auth::WEB_OPERATOR_USER_ID`, the admin-bearer identity,
  pre-collapse.
- **the OS username** — the TUI passes it straight through.

So the id encodes *auth surface × era*, and a schema change or a re-pair mints a
new one for the same person. Several of them differ only by which of the same
human's devices was holding the pen. Subagent sessions inherit their parent's,
whatever it happened to be.

## Why this bites

Any feature that says "this person's X" and reaches for `user.id` gets a
partition that is wrong in both directions:

- It **splits one human** across ids — the operator's own conversations land in
  several buckets, and a cron job's fires land in a bucket the operator never
  types under.
- It **does not separate different humans** in the one case where that matters.
  A conversation relayed by a bot belongs to a stranger; what marks it is
  `ChannelKind::Multiplexed` (`crates/channels/src/kind.rs`) — one sidecar
  connection carrying every session of its type. `user.id` happens to look
  distinctive there only because the sidecar pastes the platform's id in.

## Where this came up

The dream pass (`Router::dream_groups`,
`crates/agent/src/actor/router/cron.rs`) originally scoped its digest with
`sessions_with_user_messages_since(&event.user_id, …)`, reasoning that a
multiplexed channel relays people who are not the operator. Against a real
store that predicate hid most of the human-active sessions from the pass — it
inherits the cron job's frozen device id, so it would have seen the phone's
conversations and not the ones typed on the web.

The query is now unscoped, and `SessionTranscriptReader` does no identity check
either: the deployment serves one person, so the honest model is "all of it is
theirs" rather than a filter that is mostly wrong.

## What it costs today

The built-in memory pass is the surface where this stops being theoretical.
`dream_candidates` selects every conversation with unconsolidated rows and
does not filter on `ChannelKind::Multiplexed`, so on a deployment with a
sidecar channel a relayed stranger's conversation is read and distilled into
the operator's memory tree — and the pass's own instruction invites promoting
durable facts into the shared `personas/USER.md`, which every agent loads in
full on every turn. `SessionTranscriptReader` likewise checks nothing, so a
relayed turn can read any transcript whose id it learns.

That is documented as an accepted limitation under today's single-human
assumption (`docs/modules/memory-builtin.md` § Known limitations), not as
something the identity work has to land before. The point here is that the
channel axis — not this field — is what would fix it.

## What a fix looks like

Not "pick a better string". The shape to land on:

1. **A real principal.** One stable id per human, minted once and stored, with
   auth surfaces (admin bearer, device token, TUI, sidecar) resolving *to* it
   rather than each inventing one. The device public key stays where it
   belongs — as a credential, not as an identity.
2. **Backfill or accept.** Existing rows carry the old strings. Per
   [no legacy-data cleanup migrations](../../CLAUDE.md), the default is to leave
   them inert; if a surface needs history to resolve, that is an explicit,
   user-visible remap, not a silent sweep.
3. **`cron_jobs.user_id` stops being an identity snapshot.** A job belongs to
   the deployment (or, once agents are the unit, to an agent); a fire should not
   re-stamp a credential captured months ago.
4. **Keep the channel axis separate.** "Which human" and "which surface" are two
   questions. `ChannelKind::Multiplexed` already answers the second one
   correctly — anything relaying strangers is knowable without the first.

Until (1) exists, treat `user.id` as provenance and nothing more: fine in a log
line, never a predicate.
