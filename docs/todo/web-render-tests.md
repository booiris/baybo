# Web — nothing renders in a test

## The gap

`app/web` has **28 `.tsx` files, 15,515 lines, and zero tests that render any of them.** The 11 test
files import pure helpers and assert on data:

| tested (pure functions) | untested (every component) |
|---|---|
| `slashCompletion` `syncApply` `composerSend` `cronActions` `workBlock` `turnSync` `trace` `sessionBuckets` `outboxStore` `syncCursor` `inputHistory` | `ChatPage.tsx` (5,507) `TraceSessionPage.tsx` (1,813) `CronPage.tsx` (1,227) `SessionSidebar.tsx` (1,075) `AgentsPage.tsx` (826) `LlmPage.tsx` (769) `AnalyticsPage.tsx` (632) … |

There is also **no eslint** in `app/web` — no config file, no dep.

So the only gates a frontend change passes are `tsc -b` and `vite build`: it type-checks, and it
bundles. Neither has an opinion about whether anything appears on screen.

## What that costs, concretely

The cron-group collapse change (2026-07-17) shipped this:

```tsx
const groupCollapsed = resolveCollapsed(collapsed, key);   // renamed
…
{!isCollapsed ? (                                          // missed by the rename
```

`isCollapsed` was by then the imported **function**. `!fn` is `false`, always, so the cron fires never
rendered — collapsed or expanded. The user's whole cron block went blank.

It got through because:

- **`tsc` passed.** `!someFunction` is legal TypeScript.
- **All 146 tests passed.** None of them render JSX.
- **No eslint.** `@typescript-eslint/strict-boolean-expressions` flags exactly this.

It was caught by a human looking at the screen. That is the current test strategy for 15,515 lines.

The same shape recurs: `{count && <Badge/>}` renders a literal `0`, `{items.map()}` on an undefined
array, a handler wired to the wrong prop, a conditional that silently never fires. All type-check.

## Order to fix it, cheapest first

### 1. eslint — one PR, catches a whole class, no test-writing

`@typescript-eslint/strict-boolean-expressions` alone would have caught the bug above. Add
`no-unnecessary-condition` and the `react-hooks` rules while there. This is the highest ratio of
regressions-caught to effort in this list, and it needs nobody to write a single test.

Wire it into CI beside `tsc`. Expect a first-run backlog on 15k lines — land the rules with
`--fix` plus targeted disables rather than a heroic cleanup.

### 2. React Testing Library — render the things

`vitest.config.ts` already sets `environment: 'jsdom'`, so this is dev deps only:
`@testing-library/react`, `@testing-library/user-event`, `@testing-library/jest-dom`.

Start where the render logic is dense and the props are few, not with `ChatPage`:

- **`SessionSidebar`** — the folder tree, cron groups, collapse defaults, pin/hide slots, the search
  mode switch. It takes 17 props and 5 contexts (folder / queue / auth / router / dnd), so the first
  test costs real setup — but it is where the conditional rendering lives, and it is what broke.
- **`QueuePanel`**, **`SearchPanel`** — small, few props, own their own state. Cheap first wins.
- **`ChatPage`** last, if ever: 5,507 lines and a mock surface of router + auth + REST + WS. Its pure
  helpers are already extracted and tested; that was the right call and should continue.

**dnd-kit caveat:** pointer drag does not work in jsdom. Reorder is reachable only through the
keyboard sensor or by invoking `onDragEnd` directly.

### 3. Playwright — only for what jsdom cannot do

Real dnd-kit drag, real reload restoring localStorage, real gateway. Point it at a stub-LLM gateway so
completions are deterministic (`docs/testing.md`, `test-support` fakes). Highest fidelity, highest
maintenance. Reserve it; do not start here.

## Keep extracting pure functions regardless

`decideComposerAction`, `bucketSessions`, `resolveCollapsed`, `classifyQueueFrame` — pulling a decision
out of a component and testing it as data-in/data-out costs zero new deps and is how most of the real
rules got covered. It does not cover the wiring (the bug above was wiring), so it is a complement to
the layers above, not a substitute.

## Related

- [`web-interjection-queue-e2e-tests.md`](web-interjection-queue-e2e-tests.md) — the same problem
  worked through in depth for **one** feature, with the `ChatWs` mock seam (`onFrame` / `onStatus` /
  `sendMessage`) that makes turn completions deterministic without a backend. Read it before writing
  the first RTL test; it has already solved the hard part.
- `docs/webui.md` — build, codegen, design tokens.
- `docs/web-chat.md` — what the chat UI actually does, i.e. what the tests would assert.
