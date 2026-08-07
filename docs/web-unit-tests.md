# Web unit tests (`app/web`)

The dashboard's vitest suite — **34 files, 560 tests** — and the conventions that
keep it fast, deterministic, and dependency-light. Read this before adding a
`.test.ts` under `app/web/src`.

Scope: `app/web` only. `app/ios/web` is a **separate** pnpm workspace with its own
vitest suite over the iOS transcript reducers, nominally gated by the `ios-web` CI job
(currently `if: false`) — it
follows the same pure-logic style but is not covered here.

For **raising** coverage further — the remaining components, drag, real reload —
see the roadmap at the end.

## Two layers: pure reducers (most of it) + a thin render layer

Most of the suite imports a **pure function** and asserts on data in / data out —
no DOM, no component. That is the default and where new logic should go: pull the
decision out of the component (see the extraction pattern below) and test it as
data. `vitest.config.ts` runs under `jsdom` so a module under test can transitively
import JSX without a transform error.

A **small React Testing Library layer** covers the surfaces whose behaviour lives
in the wiring/DOM — `pages/chat/queueStore.test.tsx` (`renderHook` over the store),
`pages/chat/QueuePanel.test.tsx` (`render` + `user-event`), and
`pages/chatMarkdown.test.tsx` (`render` over `MarkdownBody`: no pure-function test
can prove the remark/rehype plugins are actually attached to `<ReactMarkdown>`).
Reach for it only when the behaviour cannot be reduced to a pure function: a render
test carries a mock surface, `localStorage`, and per-test cleanup, and is slower
than a reducer test.

Two files assert on **source text** rather than behaviour, because nothing else can
see the invariant: `pages/chatMarkdown.test.tsx` pins the `katex.min.css` import in
`main.tsx` (drop it and every equation silently renders twice — jsdom applies no
stylesheets), and `pages/chat/mathDelimiters.port.test.ts` pins the math normalizer
byte-identical to its `app/ios/web` original, which no CI job runs. Use the shape
sparingly; it is for invariants that live between files, not inside one.

Why the split matters: the pure layer alone cannot see a **wiring** bug — a
conditional that never fires, `{count && <Badge/>}` printing a literal `0`, a
handler on the wrong prop. `tsc` and the reducers both pass it. Two gates close
that hole now: the render layer (for the covered surfaces) and **eslint**
(`strict-boolean-expressions`, `no-unnecessary-condition`, `react-hooks`) for
everything else. Extending the render layer to the other components
(`SessionSidebar`, `SearchPanel`) is the roadmap below.

## The extraction pattern — what makes logic testable

The suite is downstream of a deliberate, ongoing move: **lift the hard decision out
of the component into an exported pure function, leaving the component/hook a thin
wrapper.** Pulling a decision into data-in/data-out costs zero new deps and is how
essentially every real rule got covered. Two shapes:

- **Named exports from a page component.** `ChatPage.tsx` exports
  `decideComposerAction`, `applySlashCompletion` / `caretOnSlashToken`,
  `routeInboundFrame`, `applyTurnState`, `applySyncMerge` / `applySyncReplace` /
  `transcriptItemToRow`, and the work-block folds (`closeActiveWork`,
  `settleActiveWork`, `pushToolStartedStep`, `applyToolCompletedStep`,
  `workBlockDisplay`, …). `CronPage.tsx` exports `actOnCronJob`, `cronEditPatch`,
  `jobToEditForm`, `fetchCronJobs`, `mutationErrorSlot`, … The 5,507-line
  `ChatPage` is never rendered in a test; its extracted core is.
- **Dedicated colocated helper modules.** `pages/chat/outboxStore.ts`,
  `inputHistory.ts`, `syncCursor.ts`, `sessionBuckets.ts`, `searchSnippet.ts`,
  `types/trace.ts` — each a self-contained module the React layer calls into.

A hook wrapping one of these (`useInputHistory`, `QueueProvider`) is **not** the
unit under test; the pure core it delegates to is.

## Layout & naming

- Colocated `src/**/*.test.ts(x)` sitting next to the source it exercises — the
  vitest `include`. Name the file `<subject>.test.ts`.
- `describe` names the unit. **Most files open with a header comment** stating the
  invariant being pinned and citing the upstream **source of truth** it mirrors, so
  a future edit knows what contract the test is defending:
  - the TUI (`crates/tui/src/app.rs`) for the composer ring, slash completion, and
    send-vs-park (`inputHistory`, `slashCompletion`, `composerSend`);
  - the Rust cron semantics for `cronActions`;
  - the sync protocol (`docs/sync-protocol.md`) for the cursor and thread merge
    (`syncCursor`, `syncApply`, `turnSync`).

## Determinism — time is a parameter

No `Date.now()` / `Math.random()` inside the reducers; the **caller passes the
clock**, mirroring the workspace's Rust rule. `OutboxStore.beginSend(sid, entry,
t0)` and `dueForBlindResend(entry, now)` take an explicit `now`; `cronActions`
fixes `const NOW = Date.parse('2026-07-14T12:00:00.000Z')`. Tests feed explicit
timestamps and assert the **exact** transition (`dueForBlindResend(e0, t0 +
ECHO_TIMEOUT_MS) === true`), never a wall-clock-dependent one.

## Fixtures & doubles — all local, no shared framework

No shared fixture module, no MSW, no jest module-mock graph. Each file carries its
own small factories and doubles:

- **Local factories** build the shapes under test: `msg()` (`syncApply`,
  `turnSync`), `job()` (`cronActions`), `toolResultRow()` (`trace`).
- **API types come from the generated schema**, not hand-written mirrors:
  `import type { components } from '../api/schema'` →
  `components['schemas']['ChatTranscriptItem']`, with fixtures cast `as ApiItem`.
  So an OpenAPI change (`pnpm gen:api`) breaks stale fixtures at compile time.
- **Globals via `vi.stubGlobal`**: `installMemoryStorage()` models the four
  `localStorage` methods the outbox touches over a `Map`, paired with
  `afterEach(() => vi.unstubAllGlobals())`.
- **The one network-touching suite fakes the transport, not the client.**
  `cronActions.test.ts` builds the **real** `openapi-fetch` client
  (`createClient<paths>`) and hands it a `fetch` override routed to an in-memory
  `FakeGateway` that mirrors the Rust cron semantics (soft delete keeps the row,
  pause/resume flip `status`, resuming an elapsed one-shot 400s). Because the real
  client runs, the production route paths, the `?deleted=` query, and the response
  decoding are all exercised — only the wire is faked. **This is the template for
  testing any API helper.**
- **The render layer wires two extra doubles** (`src/test/`): `setup.ts` (a vitest
  `setupFiles` entry — jest-dom matchers + `afterEach(cleanup)`) and
  `memoryStorage.ts` (`installMemoryLocalStorage()` — jsdom's `window.localStorage`
  is an inert `{}`, so a `Map`-backed one is installed on `window` without stubbing
  the whole object, keeping `dispatchEvent` / `StorageEvent` live for the cross-tab
  path). Render tests mount their subject inside a real `<QueueProvider>`.

## Running

```bash
pnpm test                                  # vitest run (what CI runs)
pnpm test:watch                            # vitest, watch mode
pnpm test src/pages/workBlock.test.ts      # one file
pnpm test -t 'blind resend'                # filter by name
pnpm lint                                  # eslint src (the wiring-bug gate)
pnpm type-check                            # tsc -b --noEmit
```

Node 24, pnpm 9 (matches CI). The full suite runs in ~2s.

## CI

The root **`frontend` job** (`.github/workflows/ci.yml`) gates `app/web`. From the
root pnpm workspace (`pnpm-workspace.yaml` includes `app/web`) it runs, in order,
`pnpm -r --if-present run build` (which for the dashboard is `gen:api` → `tsc -b` →
`vite build`), then **`pnpm --filter baybo-web lint`** (eslint — see below), then
`run check` and `run test` (this vitest suite). So four gates cover the dashboard:
it compiles (`tsc`), bundles (`vite`), passes eslint, and passes the suite.

**eslint** (`app/web/eslint.config.js`) is the wiring-bug gate: `strict-boolean-
expressions` (with `allowNumber: false`, so `{count && …}` is rejected),
`no-unnecessary-condition`, and `react-hooks/rules-of-hooks`. The 15k-line legacy
backlog is recorded in `app/web/eslint-suppressions.json` (ESLint bulk
suppressions) so only **new** violations fail; burn it down with
`eslint src --prune-suppressions`. Run it locally with `pnpm lint`.

Draft-PR trap (see the root `CLAUDE.md`): every CI job is gated on `draft ==
false`, so on a **draft** PR the `frontend` job is *skipped* and `gh pr checks`
still exits 0 — skipped is indistinguishable from green at a glance. Confirm the job
says `pass`, not `skipping`, before trusting it.

## Suite inventory

| Test file | Module / exports pinned | What it asserts |
|---|---|---|
| `pages/chat/inputHistory.test.ts` (12) | `inputHistory.ts` — `appendHistory`, `historyPrev`, `historyNext`, `HISTORY_CAP` | Composer Up/Down history ring: trim, dedup-consecutive, cap, walk/clamp/drop-to-draft. Port of the TUI `remember`/`history_*`. |
| `pages/chat/outboxStore.test.ts` (6) | `outboxStore.ts` — `OutboxStore`, `dueForBlindResend`, `resendExhausted` | Two-stage confirm (echo→sent→durable-release), blind-resend window + cap, rebase-unknown park/resume, sticky-failed + manual retry, reload persistence. |
| `pages/chat/searchSnippet.test.ts` (12) | `searchSnippet.ts` — `queryChunks`, `snippet` | Transcript-search excerpt: AND-tokenize the query the way the server does, build the highlighted segment run. |
| `pages/chat/syncCursor.test.ts` (5) | `syncCursor.ts` — `advanceFromSync`, `advanceFromLive`, `INITIAL_CURSOR` | Cursor is max-wins (never regresses) with a rebase-dirty flag. |
| `pages/chat/sessionBuckets.test.ts` (20) | `sessionBuckets.ts` — `bucketSessions`, `withoutArchived`, `resolveCollapsed`, `cronCollapseKey`, `cronGroupUnread`, `collapsedByDefault` | Sidebar folder/cron grouping, collapse defaults + override, per-group unread rollup, and archived rows dropped ahead of every other rule (fed raw, so the bucketer itself is the guard). |
| `pages/chat/mathDelimiters.test.ts` (14) | `mathDelimiters.ts` — `normalizeMath` | Assistant LaTeX normalized for `remark-math`: prose money stays literal, AMS `\(…\)`/`\[…\]` rewritten, own-line `$$` promoted (never inside a list/table), code masked, no super-linear time. Ported verbatim from `app/ios/web`. |
| `pages/chat/mathDelimiters.port.test.ts` (3) | the `mathDelimiters` pair + `syncCursor.ts` ⇄ `transcript/cursor.ts` | Each web copy stays byte-identical to its `app/ios/web` original past the header — the only gate on a module duplicated across two pnpm projects. The cursor is here because web and device run the ONE sync loop: a rebase-dirty rule that holds on only one client loses rows. |
| `pages/sessionPatch.test.ts` (6) | `ChatPage.tsx` — `applySessionPatch` | Archive merges in place (the row stays in state so a sparse unarchive patch has something to land on) while `hidden` still removes the row; cron grouping + group pin survive an unrelated patch. |
| `pages/composerSend.test.ts` (6) | `ChatPage.tsx` — `decideComposerAction` | send / park / stop / noop rule; a non-empty queue never stalls an idle submit (the regression). |
| `pages/slashCompletion.test.ts` (8) | `ChatPage.tsx` — `applySlashCompletion`, `caretOnSlashToken` | Slash Tab-completion replaces the command token, lands the caret past the trailing space, preserves args. |
| `pages/syncApply.test.ts` (6) | `ChatPage.tsx` — `transcriptItemToRow`, `applySyncMerge`, `applySyncReplace` | Row keying by server id; REPLACE re-overlays an unconfirmed optimistic send; MERGE appends + reconciles by `platform_msg_id`, no dup bubbles. |
| `pages/turnSync.test.ts` (21) | `ChatPage.tsx` — `applyTurnState`, `routeInboundFrame` | Server-authoritative `turn_state`: the Working box opens/closes only from the server, never inferred from replayed steps. |
| `pages/workBlock.test.ts` (42) | `ChatPage.tsx` — work-block folds (`closeActiveWork`, `settleActiveWork`, `finalizeTrailingAnswer`, `pushToolStartedStep`, `applyToolCompletedStep`, `markStep*`, `workBlockDisplay`, `formatWorkedLabel`, `isStopCommand`, …) | The work/reasoning-block state machine: tool start/complete steps, approval gating, cancel labelling, the `Worked …` summary. |
| `pages/cronActions.test.ts` (22) | `CronPage.tsx` — `actOnCronJob`, `fetchCronJobs`, `updateCronJob`, `deleteCronJob`, `cronEditPatch`, `jobToEditForm`, `isoToLocalInput`, `mutationErrorSlot`, … | Cron page API helpers driven against a `FakeGateway` (real `openapi-fetch` client): soft delete, pause/resume, expired-one-shot resume 400, partial-patch reschedule, error slots. |
| `pages/queueDrain.test.ts` (19) | `ChatPage.tsx` — `classifyQueueFrame`, `canBatchDeferred`, `hasSendableContent` | The interjection-queue drain decision: fire / fire-deferred / restore-deferred / pause-cancelled / pause-error / none across message / turn_state / notice frames, plus the deferred batch threshold. |
| `types/trace.test.ts` (3) | `types/trace.ts` — `resolveToolCallOutput` | Resolve a tool-call's output from the persisted result row by `tool_use_id`. |
| `components/trace/traceFormat.test.ts` (10) | `traceFormat.tsx` — duration/token formatting, `stepSummaryText`, `compressionTokens`, kind visuals | Per-kind row text and the compaction `before→after` figures the overview and the step summary must agree on. |
| `components/trace/traceTreeModel.test.ts` (42) | `traceTreeModel.ts` — `turnLabels`, `turnRollup`, `failureCount`, `neededTurnIds`, `isExternalAgentTurn`, `partitionTranscript` | Tree defaults + failure roll-up; the external-agent rule on both sides of the wire marker (a marked session counts while LIVE and while unfetched — its tree is never coming — an unmarked one still needs a terminal, stepless turn); and the timestamp partition that assigns turn-less transcript rows to turns. |
| `components/trace/transcriptModel.test.ts` (32) | `transcriptModel.ts` — `buildTranscriptNodes`, `transcriptPreview`, `transcriptSearchText` | An external agent's transcript projected into trace rows: a `ToolUse` folded with the `ToolResult` that answers it rows later, in-flight calls, orphan results, stable `ordinal:block` ids, thinking/redaction, attachments, and a search corpus that keeps a call's arguments after its result lands. |
| `components/trace/traceForest.test.ts` (23) | `traceForest.ts` — `buildForest`, `sessionHasFailure`, `sessionIsLive`, `liveSessions` | Subagent lineage indexed by attach point (span, falling back to turn), cross-boundary failure roll-up, and **cycle safety** — a mutual-parent lineage must terminate, which the same walk without its `seen` guard does not. |
| `components/trace/overviewSync.test.ts` (32) | `overviewSync.ts` — `maxOrdinal`, `pollCursor`, `mergeOverviewPage` | The incremental trace-overview poll: cursor selection (including the different-session guard), append vs replace, and the supersede-watermark move that forces a full reload. |
| `pages/chat/queueStore.test.tsx` (9) · **render** | `queueStore.tsx` — `QueueProvider`, `useSessionQueue`, `useQueueStore` | `renderHook` over the store: FIFO append, synchronous `clearPause`→`popTop` compose, `normalize` pause-collapse, reorder, defer/restore, localStorage round-trip, cross-tab `storage` ingest. |
| `pages/chat/QueuePanel.test.tsx` (8) · **render** | `QueuePanel.tsx` | `render` + `user-event`: empty-queue renders null, rows in order, send/delete callbacks, inline edit (save on Enter / revert on Esc), cancelled vs error pause banner + "Send remaining". |
| `pages/chat/ImageLightbox.test.tsx` (10) · **render** | `ImageLightbox.tsx` | The viewer's view state and dismiss rules: fit floor + 8× ceiling (and the buttons disabling at each), toolbar/keyboard zoom, reset, double-click toggle, eased-jump vs 1:1-wheel (`View.animate`), Escape / close / backdrop-vs-image click, the download filename, and body-scroll lock ↔ restore. jsdom measures every box at 0, so the zoom **percentage** can never resolve — assertions read the `transform` the state produces instead. |
| `pages/chat/AttachmentImage.test.tsx` (3) · **render** | `AttachmentImage.tsx` | The bearer-authorized blob fetch and its object URL reaching the `<img>`, the thumbnail opening `ImageLightbox` on that *same* URL with no second fetch, and the named-chip fallback on a failed fetch. |
| `pwa/availability.test.ts` (5) | `pwa/availability.ts` — `pwaBlocker` | Why the PWA is inert, and the ordering that matters: `ServiceWorkerContainer` is `[SecureContext]`, so an insecure origin ALSO has no `navigator.serviceWorker` — checking the API first reported "unsupported" (a dead end) for every LAN-IP visit instead of "insecure origin" (fixable), and silenced the one log line that would have said so. |
| `pwa/updates.test.ts` (7) | `pwa/updates.ts` — `ServiceWorkerUpdates` | The waiting-worker handoff, whose two rules fail silently and only in a real browser: an `installed` worker prompts only when the page already had a controller (else every first visit is told it is out of date), and `controllerchange` reloads only after the user accepted — it also fires when a first worker `clients.claim()`s the page. Plus snapshot identity, which `useSyncExternalStore` re-renders on. |
| `pwa/install.test.ts` (5) | `pwa/install.ts` — `InstallPrompt` | The deferred `beforeinstallprompt` is spent exactly once (a second `prompt()` on the same event rejects in Chrome), the button withdraws on `appinstalled`, and repeat offers keep the snapshot identity. |
| `pages/chatMarkdown.test.tsx` (14) · **render** | `ChatPage.tsx` — `MarkdownBody` | The math pipeline end to end (normalize → remark-math → rehype-katex → KaTeX): inline vs display, both delimiter styles, money left literal, a malformed formula colored from the palette instead of throwing, list/table structure intact, and the `katex.min.css` import itself. Also that `<ol start>` survives the component overrides — the marker counter reads it off the element, and jsdom computes no counters, so the attribute is the only half of that the suite can see. |

## Adding a test — the recipe

1. **Find the branch/invariant.** Default: if it lives inside a component render or
   a hook body, **lift it into an exported pure function** (or a colocated helper
   module) first and test that. Extraction is the convention, not a workaround.
   Reach for a **render test** (`.test.tsx`, `render`/`renderHook` + `user-event`)
   only when the behaviour is inherently in the wiring/DOM — mirror
   `QueuePanel.test.tsx` / `queueStore.test.tsx` (mount inside `<QueueProvider>`,
   `installMemoryLocalStorage()` if it persists). Where jsdom simply lacks a DOM
   API the component legitimately uses, install the gap from `test/domGaps.ts`
   (`installPointerCapture()`, `installObjectUrls()`) rather than making the
   component branch on its environment — and note that those two are installed
   for the whole file on purpose: Testing Library's `cleanup()` unmounts in a
   **setup-file** `afterEach`, which runs *after* the suite's own, so restoring
   them early puts the gap back exactly when React unmounts through it.
2. **Create `<subject>.test.ts(x)` next to the source.** Open with a comment naming
   the invariant and the upstream source of truth it mirrors.
3. **Feed explicit inputs and an explicit clock; assert the exact output.** Use
   `components['schemas'][...]` for API shapes, a small local factory for fixtures,
   `vi.stubGlobal` for `localStorage`/`window`, and the `FakeGateway`-over-
   `createClient` pattern for an API helper.
4. **Write it clean under eslint** — new code has no suppression baseline, so a
   `{count && …}` or a nullable-in-condition will fail `pnpm lint`.
5. `pnpm test` (green) + `pnpm lint` + `pnpm type-check`.

## Raising coverage further (roadmap)

Pure-function extraction covers the **rules** but not the **wiring** — and wiring
is where the real dashboard regressions have hidden. The layered plan, cheapest
first, and where it stands:

- ✅ **eslint** — `strict-boolean-expressions` / `no-unnecessary-condition` /
  `react-hooks/rules-of-hooks`, gating in CI with a suppression baseline (see the CI
  section above).
- ▶ **React Testing Library** — landed for `queueStore` + `QueuePanel`. Still to do:
  the leaf components `SessionSidebar` (the one that actually broke) and
  `SearchPanel`. Not `ChatPage` (its logic is already extracted). jsdom + the
  `src/test/` doubles are in place, so these are test-writing only.
- ☐ **Playwright** — not started (deliberately): only for what jsdom cannot do (real
  dnd-kit drag, real reload restoring localStorage, a real stub-LLM gateway).

Two implementation notes for whoever picks up the remaining layers:

- **dnd-kit drag doesn't work in jsdom.** For a component with sortable rows
  (`SessionSidebar`, and `QueuePanel`'s reorder), exercise reorder through the
  keyboard sensor or by invoking `onDragEnd` directly / asserting the store after —
  never a real pointer drag. That's why the current `QueuePanel` test covers
  everything *but* drag, and the store's `reorder` is unit-tested directly instead.
- **Deterministic turn completion via the `ChatWs` seam.** A full `<ChatPage>`
  render or a Playwright run needs completions that aren't slow, variable, or costly.
  `ChatPage` builds `new ChatWs({ onFrame, onStatus, … })`, and `ChatWs`
  (`app/web/src/api/chatWs.ts`) exposes the `onFrame` / `onStatus` callbacks + a
  `sendMessage` method — mock `../api/chatWs` to capture `onFrame`/`onStatus` and spy
  `sendMessage`, and the whole client flow drives with no backend. A Playwright layer
  instead points at a gateway launched with a stub/echo LLM (the `test-support` fakes
  in [`../testing.md`](../testing.md)) so completions are instant and canned.

## Related

- [`../webui.md`](../webui.md) — dashboard build, OpenAPI codegen, design tokens.
- [`../web-chat.md`](../web-chat.md) — what the chat UI does, i.e. what the tests assert.
- [`../testing.md`](../testing.md) — the workspace (Rust) test conventions.
- `app/web/vitest.config.ts` — the jsdom-as-import-sandbox config.
