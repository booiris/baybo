# Crate boundaries and the cron delivery ledger

The cron ledger change is complete. The cross-crate CI gate is still proposed.

The `kanban` branch fixed one instance of a repeating shape: `baybo-agent` was handed
`Arc<dyn ProjectStore>` + `Arc<dyn ProjectEvents>`, and grew four functions in
`router/issue.rs` that settled runs, appended timeline entries and wrote `issue.branch` —
four rules that had drifted out of `baybo-project`, each a real defect (`4a86d125`).

Code does not get *decided* into the wrong crate. It accretes wherever the caller already
holds everything it needs, because answering the next question locally costs two lines and
going back to the domain crate costs a method plus a review. So the handle you pass is the
only real bound, and a CRUD trait bounds nothing.

## 1. Cron delivery ledger (complete)

The agent router used to read and write delivery records through `Arc<dyn CronStore>`.
It also repeated the scheduler's rule for which executions to replay.
The router and origin actor now share a `baybo_cron::CronDeliveryLedger` backed by
the runtime's `CronScheduler`. Its three operations record completion, mark
delivery resolved and list completed one-shots awaiting delivery. The cron tests
and agent delivery fixtures exercise the same scheduler implementation.

`NeverShutdown` is already gated by `cfg(test)` or the `test-support` feature.

## 2. A CI gate for the rule

The check, stated: **a `dyn XxxStore` / `dyn XxxEvents` may only appear in the crate that
owns that domain.** Test modules excepted. Hang it next to `scripts/check-ts-bindings.sh`
as `scripts/check-crate-boundaries.sh` plus a CI job.

Grep is sufficient — this is a naming-convention check, not a type-graph analysis — but it
needs two pieces of hand-written data:

- **An ownership map**, because the traits are declared in `baybo-store` while the domain
  lives elsewhere: `ProjectStore`/`ProjectEvents` → `crates/project`, `CronStore` →
  `crates/cron`, `CostStore` → `crates/cost`, `TaskStore` → `crates/task`, `TurnStore` →
  `crates/turn`, `SessionStore`/`SessionFolderStore` → `crates/session`, `TraceStore` →
  `crates/trace`, `SecretStore` → `crates/security`, `DeckCardStore`/`DeckEvents` →
  `crates/deck`, `ChannelPairingStore`/`DeviceStore` → `crates/pairing`, `SkillRiskStore` →
  `crates/skills-assessor`. Ambiguous and needing a ruling: `AgentProfileStore`,
  `ChannelBotStore`, `ChannelSessionStore`.
- **An infra exemption list.** `BlobStore` is a storage primitive, not a domain — 41 uses in
  `baybo-tools` alone, all legitimate. Same likely goes for anything else that is a
  filesystem-shaped capability rather than a set of business rules.

### Do not turn it on big-bang

The original survey found ~40 distinct `(crate, trait)` pairs. Roughly half are a
domain crate using its own store and drop out the moment the ownership map is applied. That survey also found `baybo-agent` holding `TraceStore`, `TurnStore`, `SessionStore`
and `SessionFolderStore`, four stores in `query`, and about a dozen each in `gateway` and `cli`. Several of those may be
defensible (`query` is a read-only projection layer; `gateway` is the HTTP surface), but
that is a ruling nobody has made yet.

So follow the pattern this repo already uses for `app/web` lint: **a checked-in baseline
that freezes today's counts per `(crate, trait)` pair, with new entries failing.** Existing
hits get triaged — fixed, or allowlisted with a written reason — at whatever pace suits.
That way the gate starts green on day one and still catches the third instance, which is
the entire point.

Note the failure mode that baseline carries, learned from `eslint-suppressions.json`:
a per-file budget means touching a file with existing budget surfaces *all* of its
historical violations at once. Keep the baseline keyed narrowly, and make the error message
say which pair moved.
