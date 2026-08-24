# Crate boundaries — fix the cron ledger bypass, then make the rule mechanical

> **Status:** not started. Recorded 2026-08-09 while reviewing the `kanban` branch for
> module-boundary drift. The rule these two items enforce landed in `CLAUDE.md` in the
> same session (`## Architecture` → "Crate boundaries — what crosses, and what doesn't").

The `kanban` branch fixed one instance of a repeating shape: `baybo-agent` was handed
`Arc<dyn ProjectStore>` + `Arc<dyn ProjectEvents>`, and grew four functions in
`router/issue.rs` that settled runs, appended timeline entries and wrote `issue.branch` —
four rules that had drifted out of `baybo-project`, each a real defect (`4a86d125`).

Code does not get *decided* into the wrong crate. It accretes wherever the caller already
holds everything it needs, because answering the next question locally costs two lines and
going back to the domain crate costs a method plus a review. So the handle you pass is the
only real bound, and a CRUD trait bounds nothing.

The survey that found the board case found the same shape live in cron. Nothing currently
stops a third. Hence two items: fix the known one, then gate it.

## 1. The agent crate writes the cron delivery ledger past its own door

`crates/cron` built the door **and documented it as the door**:

| | |
|---|---|
| `crates/cron/src/scheduler.rs:453` | `pub async fn record_execution_completion` |
| `crates/cron/src/scheduler.rs:466` | `pub async fn mark_execution_notified` |
| `crates/cron/src/scheduler.rs:481` | `pub async fn list_executions_awaiting_delivery` |
| `crates/cron/src/scheduler.rs:451` (doc) | *"Called by the agent layer's cron waiter before it delivers the result."* |

All three have **zero callers**. The agent layer was handed `Arc<dyn CronStore>` and writes
the ledger straight past them:

| | |
|---|---|
| `crates/agent/src/actor/router/cron.rs:833`, `:1060` | the field: `cron_store: Arc<dyn CronStore>` |
| `crates/agent/src/actor/router/cron.rs:847` | `record_execution_completion` — a write |
| `crates/agent/src/actor/router/cron.rs:1134` | `mark_execution_notified` — a write |
| `crates/agent/src/actor/mod.rs:1093` | `mark_execution_notified` — a write |
| `crates/agent/src/actor/router/cron.rs:753` | `list_executions_awaiting_delivery` — a read |

Building the port does not help if the store rides along beside it. That is the lesson to
carry into item 2.

### Cost — cheaper than it looks, because the expensive part is already paid

No new domain methods are needed (all three wrappers exist), and the binary **already
builds** an `Arc<CronScheduler>` at `crates/baybo/src/runtime.rs:508` — the gateway has
been using it all along. The Router is separately handed `graph.stores.cron.clone()` at
`runtime.rs:1021` and `:1118`. So this is a handle swap, not a redesign.

All 37 `cron_store` sites outside `crates/{cron,store,storage}` were inventoried:

| | count | nature |
|---|---|---|
| production | ~18, across 5 files | field type + `.cron_store` → `.cron_scheduler`; method names and arities identical |
| `Volatile` marker | 2 | add `impl Sealed`/`Volatile for Arc<CronScheduler>` (`actor/state/marker.rs`) — 2 lines |
| test construction | 3 | `router/cron.rs:1321`, `router/issue.rs:534`, `integration-tests/src/harness.rs:553` |
| `agent_loop_e2e.rs` | 9 | **unchanged** — the harness keeps its `Arc<InMemoryCronStore>` field for assertions |

Only friction: the error type becomes `CronError` instead of the store's. Every call site
is `if let Err(e) = … { warn!(error = %e) }`, so `%e` covers both.

### Three ways to do it

- **A — swap the handle** to `Arc<CronScheduler>`. ~40 lines, 6 files, no new code. Kills
  the bug, but hands the delivery waiter all **19** of `CronScheduler`'s public methods
  (`create_job`, `delete_job`, `trigger_now`, `update_job`, …) — a smaller version of the
  same problem.
- **B — narrow port (recommended).** Declare `CronDelivery` in `baybo-cron` with the three
  methods; `impl` it for `CronScheduler`. ~65 lines total. **Cheaper in tests than A**:
  `InMemoryCronStore` impls `CronDelivery` directly, so the three construction sites become
  a one-line cast instead of building a scheduler with a `trigger_tx` and a `Shutdown`.
- **C — concede.** Delete the three dead wrappers and the doc comment that lies about them.
  Ten minutes, but it formally moves cron-ledger writes into the agent layer and throws away
  whatever intent produced those wrappers.

Take B.

While in there: `crates/cron/src/shutdown.rs:29` `NeverShutdown` is plain `pub` with no
`test-support` gate — and CLAUDE.md's own "test-only helpers must be gated" rule uses
"`NeverShutdown`-style stubs" as its example. Confirm it has no production caller, then gate it.

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

A survey of the current tree found ~40 distinct `(crate, trait)` pairs. Roughly half are a
domain crate using its own store and drop out the moment the ownership map is applied. The
rest are real and numerous — `baybo-agent` alone holds `TraceStore` (17 uses), `CronStore`
(8), `TurnStore` (3), `SessionStore`, `SessionFolderStore`; `crates/query` holds four
different stores; `gateway` and `cli` hold about a dozen each. Several of those may be
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
