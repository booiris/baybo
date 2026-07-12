# Web Interjection Queue — End-to-End Test Plan

## Context

The web `user_interjection` queue (park / defer / auto-fire / batch coalescing / reorder / inline-edit /
pause-banner, `app/web/src/pages/chat/queueStore.tsx` + `QueuePanel.tsx` + the `sendToSession` /
`drainQueueOnFrame` wiring in `ChatPage.tsx`) is **frontend-only**. Current automated coverage is
**pure-function unit tests only** — the repo's `app/web` tests (`workBlock.test.ts`, `turnSync.test.ts`,
`composerSend.test.ts`, `slashCompletion.test.ts`, `syncApply.test.ts`, and the `pages/chat/` store tests)
import pure helpers from `./ChatPage` and assert on them; there is **no React-rendering or browser
harness**. So the send-vs-park *rule* is pinned (`decideComposerAction`), but the *wiring*, the DOM,
drag, and localStorage-across-reload are not.

A regression already slipped through this gap: the enqueue rule parked every send while the queue was
non-empty, stalling the composer. It's now covered by `decideComposerAction` + `composerSend.test.ts`,
but only because the rule was extracted into a pure function.

## The crux: deterministic completion

Auto-fire keys on a **turn completion** (an assistant `message` frame). With a real LLM a completion
is slow, variable, and costs money → flaky tests. So an "E2E" for this feature should almost never run
against a real gateway + real LLM; it should **control when the completion frame arrives**. The seam:
`ChatPage` builds `new ChatWs({ onFrame, onStatus, ... })` (grep `new ChatWs`) and `ChatWs`
(`app/web/src/api/chatWs.ts`) exposes `onFrame` / `onStatus` callbacks + a `sendMessage` method. Mocking
`../api/chatWs` lets a test capture `onFrame`/`onStatus` and spy `sendMessage`, i.e. drive the whole
client flow deterministically with no backend.

`vitest.config.ts` already sets `environment: 'jsdom'`, so React Testing Library drops in with no env
work.

## Layers (cheapest → heaviest)

### Layer 1 — pure-function extraction (0 new deps, highest ROI, matches the repo)

Same move as `decideComposerAction`. Extract the auto-fire/pause decision out of `drainQueueOnFrame`
into a pure function, e.g.:

```ts
export type QueueFrameAction =
  | 'fire' // top parked item
  | 'fire-deferred' // every sendable deferred item, batched or one-by-one
  | 'restore-deferred' // deferred items move back to the parked queue
  | 'pause-cancelled'
  | 'pause-error'
  | 'none';
export function classifyQueueFrame(
  frame: Frame,
  ctx: {
    armed: boolean;
    alreadyFired: boolean;
    paused: boolean;
    hasItems: boolean;
    hasDeferred: boolean;
  },
): QueueFrameAction;
```

The deferred plane has a second pure decision worth its own helper — the batch-vs-individual choice
(`sendable.length >= 2 && sendable.every((i) => !isSlashText(i.text))`, after junk-filtering blank items),
e.g. `canBatchDeferred(items): boolean`.

`drainQueueOnFrame` keeps the side effects (call `sendToSession` / `sendBatchToSession`,
`store.removeItem` / `store.removeDeferred`, `store.restoreDeferred`, `store.setPause`) but delegates the
decision. Unit-test `classifyQueueFrame` for: armed + completion + items + not-fired → `fire`; armed +
completion + deferred → `fire-deferred`; unarmed (reload replay) → `none`; paused → `none`;
`turn_state{active:false}` with deferred items that never rode the turn → `restore-deferred`; transient
notice → `none`; notice with an empty parked **and** deferred queue → `none`; stop-cancel notice →
`pause-cancelled`; `level:'error'` notice → `pause-error` (both pause cases restore the deferred items
first); already-fired-this-turn → `none`. And `canBatchDeferred` for: 2+ plain items → batch; a slash
command anywhere in the set → individual; lone item → individual; blank items filtered out before the
threshold is applied. Deterministic, no new deps. **Do this regardless of the other layers.**

### Layer 2 — component / integration with React Testing Library

Add dev deps: `@testing-library/react`, `@testing-library/user-event` (and optionally
`@testing-library/jest-dom`). jsdom env already present.

- **Store via `renderHook`** (cheap, high value): wrap `QueueProvider`, `act()` the mutations, assert
  `items` / `pauseReason` / `localStorage`. Pins the invariants the integration review flagged:
  chained `clearPause → popTop` composes in one tick (the synchronous `queuesRef` update), clear-pause-
  on-empty (`normalize`), FIFO append, `reorder`, persistence round-trip (`loadAllQueues`), and the
  cross-tab `storage` listener.
- **`<QueuePanel>` via `render` + `user-event`**: it only needs `QueueProvider` + props, so render
  *just the panel* (not the whole `ChatPage`). Assert: rows render in order; send / delete fire their
  callbacks; **inline edit** (click edit → type → Enter → store text updated, Esc → reverted); the
  cancelled/error **banner** shows by `pauseReason` and "Send remaining" calls `onResume`. Mock `fetch`
  for blob previews (`AttachmentImage`).
- **Optional `<ChatPage>` + mocked `ChatWs`** (covers the glue, larger mock surface): `vi.mock('../api/chatWs')`
  capturing `onFrame`/`onStatus` + `sendMessage` spy; wrap render in `MemoryRouter` (`/chat/:sessionId`)
  + `AdminAuthProvider` (or mock `useAuth`/`useAdminClient`) + `QueueProvider`; mock the admin REST
  client + `fetch`. Then drive: connect → type → Send fires `sendMessage`; feed `turn_state{active}`
  then type → it parks (row appears, no `sendMessage`); feed an assistant `message` → auto-fire fires
  `sendMessage`; feed a stop-cancel notice → banner. Skip unless the glue specifically needs covering —
  it's a thin `if`, and the mock surface (router + auth + REST) is large.

**dnd-kit caveat:** pointer-drag does not work in jsdom. At Layer 2 you can only exercise reorder via
the keyboard sensor or by invoking the `onDragEnd` handler / asserting the store afterwards — not a real
mouse drag.

### Layer 3 — true browser E2E with Playwright (`@playwright/test`)

The only layer that exercises **real dnd-kit drag** (the 6-dot handle reorder) and a **real page reload**
restoring localStorage (incl. attachment-preview re-fetch). Point it at a gateway launched with a
**stub/echo LLM** (instant, canned reply) so completions are deterministic — wire a fake provider via
the existing `test-support` fakes (`docs/testing.md`). Highest fidelity, highest maintenance; reserve
for drag + reload + real-gateway confidence.

## Recommendation

Layer 1 + the Layer 2 **store (`renderHook`)** and **`QueuePanel`** tests. That covers essentially all
the feature's logic and DOM deterministically, with one new dev dep and no flaky backend. Reserve the
full `ChatPage` render and Playwright for when drag / real-reload / glue fidelity is specifically wanted.

## Already done

- `decideComposerAction` (pure, exported from `ChatPage.tsx`) + `app/web/src/pages/composerSend.test.ts`
  (6 cases incl. the non-empty-idle-queue regression).

## Related

- `app/web/src/pages/chat/queueStore.tsx` — store + hooks + persistence
- `app/web/src/pages/chat/QueuePanel.tsx` — panel / sortable rows / inline edit / banner
- `app/web/src/pages/ChatPage.tsx` — `sendToSession`, `drainQueueOnFrame`, `decideComposerAction`
- `app/web/src/api/chatWs.ts` — `ChatWs` mock seam (`onFrame` / `onStatus` / `sendMessage`)
- `app/web/vitest.config.ts` — `environment: 'jsdom'`
- `docs/testing.md` — `test-support` fakes (for a Playwright stub LLM)
