# iOS Projects tab — design (the kanban / project feature on the phone)

*Design settled 2026-08-23 (12 owner decisions + 8 improvements from a survey of shipped products); revised 2026-08-24 against merged master; **built 2026-08-24/25, P0–P8**. Canvas: artifact "Projects Tab 设计稿" <https://claude.ai/code/artifact/80f6b10a-071f-4288-b518-d354c8c1c9d2> (three pages — Flows / States & panels / Improvements — 32 artboards).*

*This document is the DESIGN — what the phone does and why. [projects-plan.md](projects-plan.md) is the build log: what each phase changed, and, more usefully, what the simulator and the type checker corrected. Where the two disagree the plan is the newer.*

## 0. What the code actually says (2026-08-24, HEAD = 306b22d7, kanban merged into master)

> **Read as of that date.** Everything under "Still missing" below has since
> been built — the pump lane, the PATCH verb, all of it. The section is kept
> because the three facts that SHAPE the phone are still true and still the
> reason this design looks as it does.

- The feature: `crates/project` (every rule), gateway `/v1/projects/*` (`crates/gateway/src/api/admin/projects.rs` + `project_team.rs`), `Frame::ProjectChanged` (`crates/wire/src/lib.rs:966`, scope = `project|board|run|timeline`), `app/web/src/pages/projects/*`. Nothing on iOS.
- Three facts that shape the phone, read out of the code rather than the docs:
  1. **There is no chat with the lead.** `0fa61d1e` deleted it; the only way to reach the lead is `@lead` in a card comment.
  2. **Boards are never pushed.** Turn pushes and approval pushes both bail on project sessions (`push/mod.rs:593-600, 704-722`); issue sessions are excluded from the chat list, `create_or_load`, and the app-icon badge. The phone learns about a board only while it is open.
  3. **Approvals are answered per card over REST**: `POST …/issues/{n}/approvals/{call_id}`, resolved by looking `call_id` up in the owner channel's live queue; the gateway self-denies after 300s. The existing iOS `ApprovalQueue` (derived from a subscribed session's frames) cannot see them.
- Already in place: the device token authenticates the whole `/v1` surface (`auth/admin.rs:134-139`); `Frame::ProjectChanged` **is on this branch** (the iOS ffi takes `crates/wire` as a path dependency, so it already decodes); the run-transcript endpoint returns a plain `ChatSessionDetail` (`projects.rs:1573-1610`), which the existing transcript webview renders unchanged.
- Landed with this document (plan P0): `IssueDto.approval_pending`, resolved off the live approval queue and empty on an archived board; a 409 when an archived board is asked to answer a prompt; a `ProjectChangeScope::Unknown` arm so a future scope costs a client the narrowing rather than the frame.
- ~~Still missing (plan P1)~~ — **built**: the ffi pump's consuming `ProjectChanged` lane (see [connection.md](connection.md#the-project-sink)) and `GatewayJsonClient::patch_json`.
- Changed on master since the design snapshot: `7fcec51d` gave `list_issues` **paging and search over the Done column** (the board fetch is no longer "the whole board in one go"); `2722f2cf` added `opened_by_agent` to IssueDto; `aed877c7` per-board model pin; `4e6417e0`/`da70b732` money ceiling → held runs.

## 0.1 Owner decisions (2026-08-23)

| # | Question | Decision | Where it lands |
|---|---|---|---|
| 1 | Markdown fidelity for description / comments | **A new WKWebView** (`issue.html`, the third Vite entry). The whole issue body goes into the webview; native keeps only the header and the composer dock | §3.3 |
| 2 | How many approval answers | **Two** (Approve / Deny) | §3.3 |
| 3 | Approvals on an archived board | **Not answerable** (archived means read-only). Landed in P0: `POST …/approvals/{call_id}` 409s an archived board, and its cards stop reporting `approval_pending`. Two things this leaves open — §0.2 | §3.5, plan P0 |
| 4 | `IssueDto.approval_pending` | **Add it** (gateway change) | Card hand glyph, Waiting-on-you approval rows |
| 5 | Mirror boards to disk | **Yes**, with REPLACE semantics | §5 |
| 6 | Create a project from the phone | **Yes** | §3.5 |
| 7 | A project-picker card screen | **Projects cards IS the tab root**; the board is a pushed screen; **there is no switcher in the top-left — changing project means swiping back to the cards** (a deliberate divergence from web decision 12) | §2, §3.0 |
| 8 | Tab badge | **Native `.badge(n)`** on **both** Projects and Chats (a deliberate divergence from web's "a tab gets a dot, never a count"). Since 2026-08-25 it counts **parked approvals only**, matching the strip — see §9.6 | §3.0 |
| 9 | Mark-read by swiping a Waiting-on-you row | **No** | §3.1 |
| 10 | Confirm before Stop | **Yes, a ConfirmDialog** | §3.3 |
| 11 | Editing the description | **Not on the phone** (2026-08-26). Shipped in v1 behind a ✎ and taken out whole — see §3.3 | §3.3 |

## 0.2 What decision 3 leaves open (raised by review, for the owner)

Refusing an archived board's approvals is enforced, but two things about it are worth a deliberate answer rather than silence:

1. **Archiving does not stop a run that is already executing** — only promotions read the flag, and the web's own archive dialog promises "running cards finish". So a run that reaches an approval gate seconds after the operator archives its board can now be answered neither way: it waits out the 300s `APPROVAL_TIMEOUT` and self-denies. That reaches the same end state a denial would, only later — but it is a live run's outcome changing because of a flag that claimed not to touch it. The alternatives are cancelling live runs on archive, or letting `Deny` through on the argument that denying releases an agent *away* from acting, which is what the read-only rule is really protecting.
2. **The rule has a second door that does not check it.** `Frame::ResolveApproval` (`gateway/src/channel/route.rs`) resolves a bare `call_id` against the same queue with no card and no board in hand. No shipped client answers a board prompt that way — issue sessions are excluded from every chat surface, and the run-transcript panel is REST — but a client subscribed to an issue's session could. Closing it means giving the channel router a project dependency, which is a bigger call than P0 should make on its own.

## 1. What the phone is for

**See what the board is waiting on you for, answer it there; everything else is reading and light writing.**

- Answer: approvals (Approve / Deny), failed runs (Run again), an agent's question (comment / Answer+unblock), runs held on a budget ceiling (raise it).
- Read: board state, a card's timeline, a run's transcript, who is busy.
- Light writes: move a card, create a card, create a project, pin, block / cancel, assign.
- **Not on the phone**: reordering within a column, five columns side by side, hiring (v2), persona files, the trace viewer, in-app diff/merge.

## 2. Information architecture

Three levels — **cards root → board → issue** — all pushed screens; back is always the edge swipe (`PopGestureEnabler`). Nothing in the top-left switches projects.

```
Projects tab root = Projects cards (wordmark header, tab bar visible)
 ├─ one card per project (most recent first) · dashed "New project" card · Show archived
 ├─ zero projects → empty state
 └─ tap a card → Board (push, covers the tab bar)
      ├─ header: [←] project name … [✎ new issue] [⋯]
      ├─ stage segmented control (5 segments, tap to switch; a segment with news wears a red dot;
      │   the bar strip itself takes a horizontal swipe to change stage)
      ├─ board row: team faces (working = ring) · budget chip (only over ceiling) · filter
      ├─ "Waiting on you" strip (current board only, only when non-empty, collapsible):
      │    approval (inline Deny/Approve) · failed run (Run again) · agent question (Answer) · unread card (open it)
      └─ the open stage's wall of cards: bands Pinned / New / Queue
           ├─ tap a card → Issue detail (push)
           │    ├─ body = issue.html webview (title · meta · description · state band
           │    │   [chips · live-run · blocked] · attachments · sub-issues · activity)
           │    ├─ the live-run line's `Open run ›`, or ⋯ → Runs → an attempt
           │    │   → Run transcript sheet (transcript webview, read-only)
           │    ├─ a chip → native pickers: status → the board's own Move sheet
           │    │   (consequences and all), priority → `PriorityPicker`,
           │    │   assignee → `AssigneePicker` (parent+stage is still v2)
           │    ├─ ⋯ → Run again · Runs (every attempt, newest first) — both
           │    │   conditional, so the ⋯ itself is: a card that has never run
           │    │   draws none. Designed also to carry Rename · Pin ·
           │    │   Block…/Unblock · Cancel/Reopen · Copy branch
           │    ├─ sub-issue / #N → child card (push; back returns to the card you came from)
           │    └─ dock (native): answer row (only when an agent is asking) ·
           │        ApprovalCardView (two answers) · composer pill
           ├─ long press → Open · Move to… · Assign… · Pin · Rebuild this card
           ├─ leading swipe → Pin · trailing swipe → Move… (full-swipe disabled)
           ├─ ⋯ → Activity (push) · Team (sheet → Agent profile) · Mark all read N · Settings (push)
           └─ ✎ → New issue sheet (document-first; the compose glyph, not a `+` — the cards root's `+` makes a BOARD one push back)
```

Pushed screens use the `ChatRoute` + `ArchivedScreen` header grammar plus `PopGestureEnabler`; sheets use the `SubagentSheet` grammar (`.sheet` + `.large`, **never** `fullScreenCover`); confirmations are hosted on `RootView` (`ConfirmDialog` / `RenameDialog`).

## 3. Screen by screen

### 3.0 Projects cards (the tab root)

- Wordmark header (like Deck / Settings), tab bar visible (a pushed board covers it).
- **Tab badge (decision 8)**: `Tab.badge(n)` — Projects = Σ of each board's `approvals + failed + unread` (`/projects/attention`, archived boards excluded); Chats = `BadgeCenter.total(index.rows)`, the same number the app icon already shows. Updated by the foreground attention poll and after every write; nothing pushes, so a locked phone's badge does not move. There is no `.badge` anywhere in the app today — verify on a simulator before building on it.
- A card is: name (archived chip) · `● N working` / `idle` · burn meter (underlined when over) · a **mini five-segment stage bar** (live counts, news dots) · team faces (working ringed, lead heavier) · the newest activity line (`/feed` first page) · a red attention count · ›.
- The last card is a dashed `+ New project`; below it `Show archived (N)` (archived cards render dimmed and open a read-only board).
- Data: `/projects?include_archived` + `/activity` (`since_ms` = **UTC midnight**) + `/attention` + each board's `/issues` (already in the mirror, so a cold start paints without the network).

### 3.1 Board (pushed)

- **One stage at a time** — the web `ColumnPage` on a phone. The segmented control carries live counts (**cancelled excluded**); a segment with unread wears a red dot; the `Pinned / New / Queue` bands only print a header when more than one is non-empty. **No `TabView(.page)`**: a row's swipe actions, page paging, and the edge-back gesture are three horizontal gestures fighting. The bar (segments + board row + Waiting strip) takes the horizontal swipe instead.
- **The segments are PINNED under the header** (2026-08-25); the board row and the Waiting strip stay in the scroll. They are navigation, not content — the list under them is one column out of five, and a control that scrolls away means dragging back to the top to change which column you are reading, on the screen whose whole shape is "one stage at a time". Only the segments: the Waiting strip is a row per parked prompt and pinning it would take half the screen. The pinned block paints **solid paper from the status bar down**, because the header's veil is a gradient that is clear at its bottom edge — right on a transcript, wrong here, where it left a sliver of a card row visible in the strip between the title and the segments.
- **Reading order = pinned → unread → `position`**, rendered only, never written back; a cancelled card is never lifted by unread (a pin does lift it); a refresh anchors scroll by row id.
- **A card is a row** (`ChatRowBody` grammar, `Theme.sys`): a 3px spine on the leading edge (urgent/high ink, medium light grey); first line `#N · ▲▲/▲/◆/▽ priority (all ink — red is only for failure) · (pin) · age · hand glyph (approval_pending) · red unread count`; two lines of title; a badge row of Blocked / ✕ Run failed (the one red thing) / ⑂ branch (only once it has a commit) / ↳ #N; a footer of assignee face + handle, **the runner's face** (a second, ringed face when the run is not the assignee's — a coordination run is @lead's), the run word `WORKING · 4m` / `QUEUED` / `HELD · 41m` (running measures from `started_at_ms`, queued/held from `created_at_ms`, nothing after it settles), and the `done/total` progress ring. A cancelled card is struck through and dimmed but still opens (Reopen).
- **The "Waiting on you" strip**: the current board only, and **parked approvals ONLY** (Deny / Approve inline; found by taking the `approval_pending` cards and reading their `events` for the `call_id`, which is bounded). *Something is waiting on you when it has stopped and cannot go on until a person answers* — and on a board that is a parked prompt and nothing else. It shipped with four kinds and was narrowed 2026-08-25: a **failed run** is over, not waiting (the card wears `✕ Run failed`); an **unread card** is news, and nobody is stopped (the card wears a red count, its segment a dot); an agent's **question** does park a run, but it is answered by writing a sentence and no sentence fits in a strip row (the card wears `⊘ Blocked`, and the answering happens on the card where the writing happens). Each of the three already said itself on the card row, so the strip was a third place for the same fact, filling with rows whose only affordance was "go and look" — which is the list underneath it. No countdown (the 300s timeout is a gateway-private constant); answering a prompt that is gone returns 404 → the row stays retired and the board refetches.
- **The board row**: team faces (capped at `TeamFaces.maxFaces`, with a `+N` disc for the rest — the cap used to drop the remainder silently, so a team of six drew as five; a `+1` is never drawn, since the disc costs exactly the face it would replace) → budget chip (`⏸ $6.10 / $5.00`, only when `burnState == over`, opens Settings; **a standing condition, not news, so it never feeds a red dot**) → filter chip (ink-filled with a count when narrowed). **The ⋯ is in the header**, beside `+` (Activity · Team · **Mark all read N** · Settings): every entry behind it acts on the whole board, and standing next to the filter it read as one more way to narrow the list.
- Pull to refresh (the `RefreshRing` beside the title); on a first open with no mirror, skeleton rows in the real rows' geometry.

### 3.2 Moving a card (Move sheet / long press / swipe)

No dragging. A move is `POST …/move {status, ordered_numbers}` (the destination column's **full** order, the card appended at the end). **Every row states its consequence before you tap it**:

| Target | Card has an unsettled run | Nobody assigned | Otherwise |
|---|---|---|---|
| Backlog / Todo / Review | "The run keeps going — only Stop ends it" | — | — |
| In Progress | current | **"Needs an assignee first" → tapping opens the assignee picker, then moves** | "⚡ Starts a run: @h reads the card now"; over ceiling: "may be held — over the daily {money\|token} ceiling" (the server decides at enqueue) |
| Done | "Reclaims the worktree when the run settles · nothing runs again" | — | as above |

Moving out of In Progress **never kills the run**; Stop is the only kill switch. **A move that starts no run gets an Undo toast (3s, the reverse move); a move into In Progress reports `Queued for @h — #N` and gets no Undo** — undoing it would leave the run running and the toast lying. An assign or handover must not claim "Queued" either (a handover moves the run into a new session; a same-card duplicate yields `RunRefused`) — say only what the server returned. A failure rolls back and shows the server's own sentence. A manual move is never gated on the run ceiling.

### 3.3 Issue detail (pushed) — native shell + `issue.html` webview

| Layer | Owner | Contents |
|---|---|---|
| header | native (`ChatHeaderView` grammar) | back · `#N · status` glass pill (tap → Move sheet) · ⋯ (Run again · **Runs**), drawn only when it has an entry |
| body | **`issue.html` WKWebView** | title · one meta line · the description as plain body · the state band (three chips · live-run line · blocked banner) · attachments · sub-issues · activity (**posts and lines**, below) · a "New activity" jump pill |
| pickers | native sheets over the page | status → `MoveSheet` · priority → `PriorityPicker` · assignee → `AssigneePicker`, all writing through `ProjectsStore` |
| dock | native, inside the real `ComposerDock` | jump-to-newest disc · notice line · `ApprovalCardView` (two answers, REST-backed) · answer row (only when an agent is asking) · staged strip · the chat's composer pill, at the chat's width and beat (`+` → attach panel, in-field send, no prompt text) |
| overlays | native | pickers (`ModelMenuPanel` style) · sheets · `RenameDialog` · `ConfirmDialog` |

- Why the whole body rather than a small webview for the description alone: a webview inside a native ScrollView is two scrollers plus height round-trips, while a full-page webview is exactly `ChatScreen`'s existing layering (header / webview / dock / bottom-inset stream) — and comment markdown, attachments, `#N` links and KaTeX all come free.
- Shape: a third Vite entry `issue.html`; an `IssueBridge` (the size of `DeckBridge`, with its main-frame-only guard and 3-reloads-per-30s crash budget): native→web `init / deliverIssue / deliverEvents / deliverRuns / setBottomInset / jumpToLatest / blobResult`, web→native `ready / pick(field) / openRun / openIssue(n) / activityAtBottom / requestBlob / viewImage / previewFile / copy / log`. **Inline images ride the bridge's `requestBlob`/`blobResult` (as the transcript does), so no scheme route is needed**; putting HtmlPreview in the issue body would need `DynamicRoute` widened, which v1 skips. The keyboard: the webview never resizes; native streams the bottom inset (the transcript's mechanism). One host per card, torn down on exit.
- **`subscribeIssue` has ONE holder, and it is the React tree** (2026-08-25 —
  shipped broken, fixed same day). `main.tsx` posts `issueReady` on the line
  after `createRoot().render(…)`, which has only SCHEDULED the tree; native
  answers in that same main-actor turn with everything it holds (the flushed
  `pending` evals plus `redeliver()`), and `IssuePage`'s subscribe effect runs
  ~18 ms later. The `buffer` in `issue/bridge.ts` is what carries the card
  across that gap — and it only holds while the slot is EMPTY. `main.tsx` had
  parked a language stub in it whose `deliver` was `() => undefined`, so the
  card was handed to the stub, dropped, and never re-sent: **"Loading card…"
  forever, with the card already in the app** (the dock, which reads the same
  `IssueStore`, drew the blocked banner and the hint the whole time). It looked
  like a *direct-connection* bug only because the relay leg is slow enough that
  the fetch usually lands after the tree has subscribed; a mirror on disk
  reproduces it on any leg. `init` and `setLanguage` now have their own latched
  listeners (`onIssueInit` / `onIssueLanguage`) — the transcript's shape, which
  this page had copied everywhere except here. `deliverBeforeMount.test.tsx`
  imports the real entry module and pins it; a test that mounts `<IssuePage/>`
  first passes on the broken wiring, which is why the three existing render
  suites never saw it.
- The live-run row: running → Stop → **`ConfirmDialog`** (decision 10) "Stop run #k? The card stays where it is. Stopping is the only way to end a run."; held → `@h is held — over the daily token ceiling` + **Run it again** (on a held card the press is what releases it, so it is never greyed out); failed → `✕ Run #k failed — <error>` + Run again. Stop lives only here and in the transcript header, never in a long-press menu.
- **The composer hint is gone from the phone** (2026-08-26). It was the
  **third mirror** of `comments::comment_delivery`
  (`crates/project/src/comments.rs:37`) — a sentence above the field saying
  what sending would do, since that decision is not exposed over REST and a
  composer has to say it while the text is still being typed. What it cost was
  two lines of full-width 10.5px mono over every card, never localized, and
  mostly repeating the state band a thumb's width above it: a card that says
  `WORKING @dev-1` does not also need "@dev-1 is mid-run — this is picked up
  when that run finishes". The port (`CommentHint.text` / `.mention`) and the
  golden vectors that held it to `app/web`'s copy went with it;
  `CommentHint.swift` is now `AgentHandles.swift`, holding only the handle
  lookup three surfaces share. **The web's copy went too, the same day**: there
  the sentence was never on the page at all — it was the send button's `title`,
  a hover tooltip nobody had seen — so `commentHint`, `mentionHint`, the
  generator and the shared vectors are all gone, and the rule is back to ONE
  implementation, the gateway's. A client that wants to warn in advance again
  needs a mirror and a fixture in the same commit. What stays on the dock is the
  ANSWER row, which is a control rather than a caption: it carries "Unblock #N
  after sending".
- **The approval card**: `ApprovalCardView` unchanged in the dock (`CompactPillButtonStyle` lifted out of its file first), two answers; the pending set is the card's `events` replayed by `call_id` (requested without resolved). The live queue is the truth, so tolerate a 404 on answer.
- **The Answer flow** (an agent's question): Answer from the Waiting strip or the blocked banner opens the card with the composer focused and `@lead ` prefilled, the answer row reading "Answers @lead" beside "Unblock #N after sending", checked by default → `POST comment` first, then `PATCH {blocked_reason: null}` (the unblock door hands the parked run back out, and its brief carries your answer). A block the operator wrote themselves does not get this treatment.
- **Rebuild is on the LIST, and the description editor has no door**
  (2026-08-26). The card's ⋯ had two unconditional entries and both went. The
  hatch belongs to the board row's long press: a card whose local copy is wrong
  is a card whose own chrome you have just stopped trusting, and from the list
  it costs no round trip to reach and none to leave. `IssueStore.discardMirror`
  is the whole of it from there — there is no page to reload and no memory to
  clear, so the next open IS the cold-open path — and the toast is the
  feedback, because the row does not change and a press with no visible answer
  is a press that did nothing as far as anyone can tell.
- **The description editor is GONE** (2026-08-26), not merely doorless: the
  flag, the dock's Cancel|Done bar, the bridge's `setEditing` and
  `descriptionDone`, the page's `<textarea>` and its CSS, `IssueStore`'s
  `PATCH {description}` and its three strings all came out. It shipped in v1
  as decision 11 — a ✎ in the ⋯ swapped the rendered block for the raw
  markdown and the dock became "Editing description · Cancel | Done". A card's
  text is written by whoever files it and by the agent working it; the phone
  reads it, comments on it, and does not rewrite it. Renaming a card was never
  part of this and is still `RenameDialog` (⋯ → Rename) on the list.
- **The head is the title and the text; state is read second** (2026-08-26).
  The three pickers sat between the title and the card's first sentence, so
  the first screen was a title, a row of pills, a line of provenance and then,
  if there was room, the description. A card is opened to find out what it is
  called and what it says: those two are now adjacent, and the chips, the
  live-run line and the blocked note follow as one `.issue-state` band under
  the text.
  - **The chips carry a hue**, and it is the app's one departure from
    ink-on-paper (`docs/design-system.md` § Colour is for STATE). A status and
    a priority are what a board is scanned for and are read in a glance rather
    than a sentence, and in ink-soft a card in Review looked exactly like one
    in Backlog until you read the word. Muted, keyed by the VALUE
    (`[data-status]` / `[data-priority]`) so the chips and the sub-issue dots
    read ONE table, and tinted rather than filled — each chip is a button that
    opens a picker, and a solid coloured pill reads as a thing to press for
    consequence. A value that is not in the table keeps the neutral ink-soft:
    `backlog`, and priority below High.
  - **The chips became controls** in the same change. All three posted `pick`
    and native dropped it — `picking` was written and never read — so status,
    priority and assignee were inert from the day they were drawn, which
    colouring them only made louder. They now raise the board's own
    `MoveSheet` (consequence rows and all), a new `PriorityPicker`, and
    `AssigneePicker`, including the chain the board has: In Progress with
    nobody on it opens the assignee picker first and finishes the move on its
    answer. A picker swiped away without an answer clears that pending move —
    the board latched it, so the NEXT assignment carried the card to a column
    nobody asked for (fixed on both screens).
    - **The writes go through `ProjectsStore`, not the card's own store.** A
      move sends the destination column's WHOLE order and a move into In
      Progress starts a run; those rules have one home, and the card page is
      not it. What the card does after the board answers is refetch — it holds
      a SECOND copy of the same row, so without that the chip the operator
      just changed keeps saying what it said. A refusal is carried over
      verbatim onto the card's banner (`IssueStore.showWriteError`), because
      the board's sentence names which ceiling and which block. `overCeiling`
      / `heldCeiling` come from `ProjectsStore.budgetMeter(board:)`, lifted off
      the board screen so the two surfaces cannot disagree about which ceiling
      is biting.
    - **Priority is the one card verb with no consequence**: it orders no
      column and starts no run, so `PriorityPicker` wears `AssigneePicker`'s
      grammar (a mark, a word, a tick) rather than `MoveSheet`'s. Its
      `ProjectsStore.setPriority` PATCHes ONLY `priority` — the patch is a
      full replace of every field it names, so `ProjectsStoreTests` pins the
      shape of the body rather than the board that comes back.
    - **No undo toast here**, unlike the board: the Move sheet's row already
      said what the press would do, a move from the card is reversed by moving
      it back, and the toast machinery belongs to the list screen.
  - **The run LIST moved into the native ⋯** as a `Runs` submenu, newest
    first. A settled attempt is history — read once, when something went wrong
    — and it sat between the card's state and its comments on every open. An
    attempt with no session is listed and DISABLED rather than hidden: the
    difference between "there was no third attempt" and "the third attempt
    never got a slot" is the whole reason to open the log. A failed attempt
    carries the server's sentence as the row's SUBTITLE — the one thing the
    deleted list said that nothing else does — and that subtitle is DRAWN and
    nothing more: a menu row exposes only its title to accessibility, so the
    sentence rides the row's `accessibilityLabel` too (`runReading`). The live
    run stays on the page, because that is not history but the card's state.
  - **The page scrolls in ONE axis** (`overflow-x: hidden` on `.issue-page`).
    `overflow-y: auto` alone computes the other axis to `auto` as well, so one
    thing wider than the reading band — a title carrying a path or an
    identifier, a `project-<ULID>/feat/<slug>` branch on the meta line, an
    agent naming a symbol in a blocked reason — panned the whole card sideways
    under the finger, native header and all. The clamp is the invariant; the
    page-wide `word-break: break-word; overflow-wrap: anywhere` beside it (the
    transcript's own `.msg.assistant` rule, which this page never got) is what
    keeps that from turning into text silently CLIPPED at the edge by `.md`'s
    `overflow-x: clip`. `ProjectCardUITests` pins the outcome rather than
    either rule: it drags sideways and asserts the title's left edge does not
    move.
- **Nothing is framed above the Activity** (2026-08-25). The head had grown
  three stacked rectangles between the title and the card's first sentence —
  a row of five identical chips, a bordered live-run box, and the description
  wearing a post's box, avatar and author bar. Each is now the lightest thing
  that carries its fact:
  - **Chips are for controls**: status, priority, assignee, and nothing else.
    A branch opens nothing and a parent is a link, so they moved to the meta
    line — four objects of equal weight saying unrelated things is what made
    the top of the page unreadable. (The chip row itself has since moved BELOW
    the description; see the 2026-08-26 entry above.)
  - **One meta line** in mono ink-soft: `opened by @who · time · ⑂ branch ·
    ↳ #parent`. It absorbed the description's old author bar, which sat under
    a title that had just said what the card is.
  - **The live run is a line**, not a box, and `Open run ›` is a text link
    rather than the page's third capsule.
  - **The description is not a post.** It is what the title is about, one line
    above it — no box, no face, no head. Every framed thing on the page now
    belongs to somebody who wrote it, which is what makes the frames mean
    something.
- **Posts and lines** (2026-08-25): the card reads as a thread. What somebody
  said — the description included, hoisted into the first box under its
  author's name — is a bordered box with a face beside it and a head reading
  `@who · time`; what the board did is one line on the same left rail, a dot
  in the face column and a sentence. The split is what keeps a long card
  readable: the eye finds the next thing a person wrote by running down the
  face column, and machinery in the same frame makes a wall of rectangles.
  - **Every agent ends up with a real picture, and one generator draws it.**
  Nothing ever set `avatar_blob_id`: every path that creates an agent passes
  `None`, and the only door that writes one is `PUT /v1/agents/{id}/avatar`.
  The web dashboard hid that by drawing a DiceBear Bottts face locally as a
  fallback — so the same teammate was a robot on the web and two letters on
  the phone.
  - **The card page draws it and native stores it.** On delivery, any
    teammate with no `avatar` gets `botttsPng(profileId)` — the same library,
    the same seed rule and the same four backgrounds `app/web` uses —
    rasterised on a canvas and handed over the bridge; native uploads the
    bytes and PUTs the id. After that both clients draw the same blob.
  - **Why not in Rust, at creation.** The gateway has no JS engine (`bun` is
    for sidecars and deck cards; a core path must not grow an optional
    binary), so generating there would mean porting somebody else's artwork —
    a second implementation that drifts on their next release. The cost of
    doing it client-side is accepted and bounded: an agent has letters until
    a card naming it is opened once.
  - **PNG, never the SVG.** A native `UIImage` has no SVG decoder, so an
    `image/svg+xml` avatar passes the gateway's `image/*` check and then
    renders as nothing on every board row.
  - Once per agent per page (the card refetches on every frame its board
    sends, and each delivery carries the whole roster), and **silent on
    failure**: nobody asked for it, so a refusal leaves the monogram that was
    already there rather than raising a banner over an untouched card.
  - A picture can also be chosen by hand: the face in `AgentProfileSheet` is
    a `PhotosPicker`. Same two calls in the same order — blob first, then the
    agent points at it, because the gateway stats the blob when the avatar is
    set and refuses a dangling reference.
- **Faces come from native**: `IssuePerson { handle, avatar, monogram }` per
    agent id on the payload. The monogram is `AgentMonogram.map`'s, i.e. the
    TEAM's — `dev-1` and `docs-1` both reduce to `D1` until the set widens, so
    a page deriving one from the handle it was handed would draw the collision
    the board already avoids. The picture rides `requestBlob` (this page's
    scheme handler is static-only) and is cached per blob id, since one avatar
    appears on every row its author wrote.
  - The operator reads as **You** and wears the one filled disc; the board is
    hairline and dashed. It printed "board" for the operator's own comments
    until this change — `actorHandle` answers `null` for a user and the system
    alike, and the row printed the system's word for either.
  - Bodies are **Inter at 0.875rem**, the one place on the page that leaves the
    monospace chrome behind: a card is scanned, a comment is read. Everything
    else came down a notch with it (title 20→17 and into Inter, matching the
    native card row; chips 11→10.5).
- **The dock is the chat's pill on shared parts** (2026-08-25). `ComposerPill`,
  `ComposerSendCircle`, `StagedStrip`, `AttachButton` and the pickers are
  store-free views both docks use; `ComposerStaging` — text, spool, uploads,
  draft — plugs into whichever surface owns it through `ComposerHost`
  (`draftKey` + `notice`), which is the one thing that made this a deferral.
  What stays per-surface is what genuinely differs: this file keeps the answer
  row, its own flat veil, and a send that posts a comment and THEN lifts a
  block.
  - **No `send` on the seam**, deliberately: a chat mints an id, paints an
    optimistic bubble, writes an outbox row and sends through a connection
    gate. The machine's door out is `claimSend()`, which also moves the send
    GATE off the view — a surface reading `staged.compactMap(\.blobId)` for
    itself would ship a comment minus every pick still uploading, silently.
  - **Only a landed comment discards.** There is no outbox here, and the picks
    are uploaded blobs: clearing the strip on a failure strands files the
    operator cannot get back. `IssueStore.comment` answers `Bool` for exactly
    this, and it is why that one verb is `async` where the rest are not.
  - The card's draft lives under `card-drafts/`, never beside the
    conversations: `AppStore.unsentDraftSessionId` treats an unlisted,
    outbox-free directory in the chat root as the abandoned new chat the
    compose button resumes.
  - A comment's files carry `filename` on the wire (`IssueAttachmentInput`) —
    the gateway resolves mime and size off the blob, but nothing there knows
    what the user picked the file AS, and the page prints file cards by name.
  - **The pill is the chat's, width and beat included** (2026-08-26). It used
    to sit at a gutter of its own and never move, on the argument that this
    dock streams its top edge to the page as a bottom inset and an animating
    dock is a moving inset per tick — but the gutters are HORIZONTAL and move
    no edge the page reads, and the cost of the exception was the same control
    being two different widths one push apart. The gutters now live in
    `ComposerPill` (40pt at rest, 14 focused, `.easeOut(0.25)`) rather than in
    either dock; the rows a dock stacks above the pill keep their own.
    The **vertical** gap followed the same day and could not move in with them:
    the card's pill sat a flat 8pt off the floor where the chat's sits flush
    and lifts 12 on focus, and that number is tied to each dock's own veil,
    which turns solid across exactly that strip. `IssueDock` carries
    `dockBottomPadding` as its own copy of `ComposerView`'s. Unlike the gutters
    it IS a height, so the 12 reaches the page as a re-pad — which is what the
    keyboard riding up does a beat later anyway.
  - **The field draws no prompt.** What a comment will do is already said by
    the card's own state band, so the grey sentence inside the pill was a
    third voice saying the obvious. `issue.commentPlaceholder` became
    `issue.commentField` and is now the field's accessibility NAME: a
    `TextField` takes its name from its prompt, so removing one without this
    leaves something VoiceOver can land on and not say what it is.
  - **The way back down** is the chat's disc, on the chat's rule: `arrow.down`
    in glass above the dock, only while the newest activity is off screen. The
    page reports that (`activityAtBottom`) on every scroll AND on every
    delivery — a card that arrives taller than its screen fires no scroll event
    at all, so a signal taken only from `onScroll` says "at the bottom" about
    the one card that needs the disc most. The dock's geometry is read on
    `IssueDock` rather than around the whole stack, so the disc popping in does
    not inflate the inset and reflow the card under a button that only
    appeared — `ChatScreen` measures its composer for the same reason.
- `POST …/read` fires only after the timeline renders successfully, then attention is refetched.
- **Every run of system events collapses** into a closed `N events ›` line — a
  run of one included — and presses open and close it again. Comments,
  approvals and blocks never collapse.
- **The card opens where the reading stopped.** `GET …/events` answers
  `IssueTimelineDto { items, first_unread }`; the page draws a red `NEW` rule
  above that entry and `scrollIntoView`s it (clearing the floating header via
  `scroll-margin-top`, not a magic offset). Everything above it is still there —
  a card is a record, and arriving mid-thread has to be scrollable out of.
  Three things make it behave:
  - **The boundary is the server's.** `first_unread` is resolved by
    `ProjectStore::first_unread_event`, off the same `UNREAD_EVENT_PREDICATE`
    the unread badge is counted with, so the rule and the badge cannot
    disagree. Neither client re-derives it — §4's last row, applied to a
    position instead of a count.
  - **The page freezes it.** Painting the card stamps it read, which
    invalidates the timeline, whose refetch answers with no boundary at all —
    a rule that tracked the payload would vanish a second after the reader
    arrived. Native mirrors that: `firstUnread` is **live-only**, never
    restored from the mirror (the same rule as `liveRun` and
    `pendingApprovals`), because a boundary replayed off disk points at a line
    already crossed.
  - **A finger wins.** The mirror paints first and the live answer lands a
    moment later; a `pointerdown` before it does disarms the scroll and leaves
    the rule.
  - The fold splits at the boundary (`fold(events, breakBefore)`): a group is
    drawn at its first member, so a swallowed boundary would put `NEW` above
    entries read yesterday. The run carrying the boundary is the one fold that
    opens by default — landing a reader on a closed line is landing them on
    nothing. It follows the payload rather than being seeded at mount: the
    mirror mounts the row a beat before the boundary exists.

### 3.4 Run transcript (sheet)

- Reuses the transcript webview through `ProjectRunReadStore: TranscriptTarget` (a `SubagentReadStore` twin: read-only by type, **`mirrored = false` kept** — decision 5's mirror is the board snapshot, never a run transcript).
- Data: `GET …/runs/{attempt}/transcript` (a `ChatSessionDetail`, paged by `before_ordinal`/`limit`). **There is no sync endpoint**: while a run is unsettled the page advances by re-reading the NEWEST page on `ProjectChanged{scope: run|timeline}`, degrading to a 2s poll if frames are unavailable, with one last read after it settles.
- **One route, two frame kinds, and the kind is what matters** (2026-08-26 — it shipped wrong). The initial load takes `project_run_transcript_baseline`, which reads the route with no cursor and dresses it as a **`sync_page`** (`since_ordinal: null`, so a baseline REPLACE); a scroll-up takes `project_run_transcript` and gets a `history_page`. Answering the initial load off the history door loses TWICE: the web drops a `history_page` that matches no in-flight backward-paging request, *and* only `sync_page`/`sync_failed` unwinds the guard its sync request armed — so a run sat on “Loading conversation…” forever with its transcript already fetched. The failure frames split the same way, and by DOOR rather than by whether the ordinal is nil: a first scroll-up has no cursor either. `gateway_api` pins what each door emits, `ProjectRunReadTests` pins which door each caller takes; neither half means anything alone. The demo has no gateway for this sheet, which is how it went unseen.
- It is one agent's session on that card (an attempt's page therefore also holds the attempts before it; the brief is a user bubble, which segments it naturally — per-run headers are v2). Read-only: no composer, no approve/deny (answer on the card); the header carries Stop (with the confirm).

### 3.5 The rest

- **Team sheet / Agent profile**: faces (working ringed, queued dimmed, lead heavier), handle, role, state `on #12 / queued #15 / idle`. The profile adds: state line · Runs on · Joined · On its plate · Remove (hidden for the lead; a busy agent shows "Busy on #12 — can be removed once idle"). **Hiring is v2.**
- **The pin, on the profile**, is the same three fields `app/web` offers (`LlmPinFields`) in this app's grammar: `llm` → `model` → `thinking`, each row opening a list inside the sheet under a back row rather than a sheet on a sheet. The rows themselves come from `LlmPinOptions`, a deliberate mirror of the web's `teamModel.ts` — one home for the four rules that fail silently when they are written twice: a model is pickable only within an entry, changing the entry drops the model (the thinking level survives, being the one level the server takes alone), a provider baybo sends no effort to gets no field at all rather than a dead one, and a pin the pool no longer knows stays on screen as `(unavailable)` instead of vanishing while the agent goes on failing on it. `PUT /v1/agents/{id}/model` replaces the whole triple on every pick; the sheet then refetches the roster and re-reads `member` from it, so what a row shows after a write is the server's answer and not a local echo.
- **Activity (pushed)**: a read-only stream; a row is a glyph, one sentence (actor / #N / the word that ends a run in ink) and a relative time; failure is the only red; tapping opens the card. Unknown timeline/feed kinds must render through a fallback — master is still adding kinds.
- **Filter sheet**: search (title or #N) · assignee · **Running only** · Unread / Blocked / Failed / Held only · Hide cancelled · Clear. The filter chip carries the count of active narrowings.
- **Project settings (pushed)**: name / description / both ceilings (with today's spend and the web's hint sentences) / parallel runs / **Agent auto-merge** / workdir (read-only) / Archive — the red outline pill the app's other destructive action uses, ink-coloured in the restoring direction. Both directions confirm, through the system `.confirmationDialog` and not the hand-rolled `ConfirmDialog` (which mounts on `RootView`, i.e. BEHIND this sheet). The system draws it with no visible Cancel, so the scrim is the only way out — and a scrim dismiss is what used to latch that dialog's binding; `ProjectsUITests` pins that it does not, here. **`PUT` is a full replace** — an omitted field clears it, and `agents_may_merge` is where that bites: it is a plain `bool` the gateway defaults to `false`, so leaving it out is not "leave it alone" but "turn merging off". Every Save from this app did exactly that until 2026-08-26, when the field reached `ProjectSettings` and the sheet grew the switch. It is named *auto-merge* rather than the web's *Agents may merge*: `may` reads as a lock and `docs/modules/project.md` is emphatic that it is not one — a run holds `Bash` and a writable checkout, so the flag decides whether the board INVITES a merge and whether `IssueMerge` opens, not whether a merge is possible. The two hint sentences are the web's `agentsMayMergeHint` reworded to the present tense, since a switch already shows its state where a checkbox needed the `On:` / `Off:` prefix. The over-ceiling chip deep-links here and highlights the ceiling that is biting.
- **New issue sheet**: document-first; chips prefilled from the open stage; `⚡ In Progress + @h — creating this starts a run`; In Progress without an assignee cannot submit.
- **New project (pushed)**: Name (required; the server creates `work/<name>`) · Description · optional ceilings and parallel runs (the web's hints) → on success, push the new board (its lead is hired with it).
- **Empty state**: zero projects → icon + a line that also states boards are never pushed + New project.
- **An archived board**: an `Archived · read only` chip; + greyed; move / comment / assign disabled and carrying the server's 409 sentence; Mark read, Stop and Unarchive still work; **approvals are not answerable** (the card renders greyed with "Archived — unarchive to answer" and the dock hosts no approval card; `/attention` already excludes archived boards, so nothing leads you to an unanswerable prompt).
- **Offline**: a line reading "Offline — the board as of 14:02", **writes disabled** (not fail-fast), no outbox for board writes (the board moves while you are away; replaying a queued write is wrong), and the mirror is what is painted.

## 4. Domain rules the phone must not break

| Rule | How the phone honours it |
|---|---|
| Entering In Progress is the only execution trigger; it needs an assignee | Move → assignee picker; New issue refuses to submit |
| Moving out of In Progress never kills a run; cancel-run is the only kill switch | "the run keeps going"; Stop only on the card and the transcript header |
| Column counts exclude cancelled; cancelled cards are struck through, filterable, reopenable, never deleted | segmented counts / Filter / ⋯ Reopen |
| Priority never reorders a column; pin and unread lifts are reading order only | never writes `position`; scroll anchored by row id |
| A manual move is never gated on the run ceiling | Move is never disabled for a full board |
| A hold is a standing condition, not an event | budget chip, never a red dot; "Run it again" is the release |
| At most one unsettled run per card; a refusal is only visible in the timeline | toasts say only what the server returned |
| An @mention on a staffed card is a question, not a reassignment; on a blocked card it staffs nobody | the gateway enforces it; no client warns in advance any more |
| A comment on a running card is picked up by a follow-up run | "picked up when that run finishes" |
| The lead cannot be removed; nor can an agent with a run in flight; a handle never changes | no Remove on those profiles |
| Archived is read-only except unarchive / mark read / cancel run; approvals not answerable | §3.5 |
| A move sends the destination column's full order | appended at the end |
| Pending approval is derived; the live queue is the truth | 404-tolerant, no countdown |
| `unread` / `last_run_failed` / `approval_pending` are resolved server-side | the client never re-derives them |

## 5. Liveness and data flow

- **The mirror (decision 5)**: `projects.json` (list + activity + attention snapshot) and one `board-<id>.json` per board (issues + active runs + team) under the support directory. Template: DeckStore's REPLACE plus SessionIndex's injected directory (so parallel tests are isolated). A cold start paints the mirror, then fetches; **the remote wins wholesale — no per-field merge**; a failed write rolls back to the mirror; logout/rebind calls `removeMirror()`. Run transcripts are never mirrored. Mind the Done paging (`7fcec51d`): a board snapshot keeps only Done's first page plus its count.
- **Fetches**: opening a board = project / issues / agents / active runs (a card adds events / runs); the cards root = projects / activity / attention / feed head.
- **Pushes**: `project_changed` on the owner WS → **any scope means the board is dirty** (`move_issue` emits no board-scope frame), debounced 300ms for the open board; a card page reacts only to its own number; a run sheet re-reads on `run|timeline`. `Gap{session_id: null}` triggers the same refresh. While a row's swipe panel is open, refreshes are held and applied on release.
- **Attention**: foreground poll every 60s, plus after every write, plus on entering the tab (keyed on `homeTab` — `onAppear` re-fires and is unreliable), plus `scenePhase == .active`, plus when `chatPath` empties.
- **Badges**: the app-icon badge still counts chat unread only; the tab badges are §3.0.

## 6. Native vs webview

| Surface | Choice | Reuse |
|---|---|---|
| cards root / board / sheets / settings / team / activity / new project | native | `ChatRowBody`, `swipeActions`, `contextMenu`, `ModelMenuPanel`, `ConfirmDialog`, `RenameDialog`, the Archived header, `PopGestureEnabler`, the undo toast, `RefreshRing` + the hand-rolled pull, `DirectLoginView`'s form shape |
| Issue detail body | webview `issue.html` | `Markdown.tsx` (its plugin order is load-bearing) + the attachment components (to be extracted from `Transcript.tsx`) + `styles.css` tokens + i18n (`issue.*` keys in both en and zh, gated by the parity test); new `IssueBridge` + `IssueHost` |
| run transcript | webview | `TranscriptHost(store:)` + `ProjectRunReadStore` |
| approval card | native | `ApprovalCardView` unchanged (`CompactPillButtonStyle` lifted), REST-backed queue |
| attachments | native viewers / inline in web | `TranscriptMedia`'s four presentations; uploads via `blob_upload_*` (`deck_card: None`) |

Rejected: putting the web board in a WKWebView (a Tailwind/brutalist re-skin, `touch-action: none` killing scroll, tests blind to paint, a 423 KB cold boot); embedding a small webview for the description alone (nested scrollers).

## 7. Testing strategy

- Pure logic lives in `Core/`: `BoardOrder` (pinned→unread→position), `MoveConsequence` (the Move sheet's sentences), `CommentHint`, `PendingApprovals` (replay by `call_id`), `BudgetMeter`, `RunLabels` (including elapsed).
- **Golden fixtures shared with app/web**, copying the whole `searchSnippetVectors.json` precedent: `app/web/src/pages/projects/commentHintVectors.json` generated by an mjs script calling the web's real `commentHint`, imported directly by web vitest, and read off disk by a Swift test that walks up from `#filePath` to the repo root (as `SearchSnippetVectorTests.swift` does, coverage canary included). `approvalReplayVectors.json` the same way. Three mirrors (`comments.rs` ↔ `timelineModel.ts` ↔ Swift) pinned by one set of vectors. **(Retired 2026-08-26** — both client mirrors and the fixture were deleted with the hint they fed; see §3.3.)
- A `-baybo-demo-projects` fixture with no gateway (hung off the `-baybo-open-home` block, short-circuited by `demoHomeMode`); `-baybo-home-tab projects` already exists.
- Accessibility: rows carry label = title, value = subtitle; any bar whose children come and go gets `.accessibilityElement(children: .contain)`; the Waiting strip's buttons need `.buttonStyle(.plain)` isolation; floating panels need pixel sampling (UI tests are blind to paint).
- New web files start with zero eslint suppressions; extracting the attachment components out of `Transcript.tsx` shifts suppression counts between files, so the baseline gets regenerated.

## 8. Scope

- **v1**: cards root; board (one stage + the four-kind Waiting strip + bands + pull-to-refresh + skeleton + run elapsed + Undo toast + stage-bar swipe + haptics); Move / long press / swipe; Issue detail (webview body + native dock: two-answer approvals, Stop with confirm, Run again, Answer+unblock, comments + @mention + attachments, collapsed activity); run transcript sheet; Team + profile (read / model pin / remove); Activity; Filter (with Running only); Settings (with archive); New issue; New project; empty / archived / offline states; the mirror; tab badges.
- **v1.5**: renaming the title, sub-issue parent/stage pickers, Copy branch.
- **v2**: hiring, persona files, board push (plus Live Activity and a longer approval timeout), project attention in the app-icon badge, per-run headers in the transcript, HtmlPreview inside an issue, a share extension that files an issue, issue search in the global Search tab.
- **Deliberately never**: drag reordering, five columns side by side, `approve_always`, a trace-viewer link, a workdir text field, diff stats, comment/attachment counts on the card face, inbox snooze.

## 9. Rejected approaches (do not re-tread)

`TabView(.page)` with five pages (three horizontal gestures fighting); an inbox as the tab root; an approval countdown; fail-fast offline writes; a "Needs you" band that duplicates whole cards; a cross-board N+1 fan-out over `events`; a small webview for the description; a tab dot instead of a count (the owner's third round replaced it with a number).

## 9. The cards root's order, and its one action

*Both settled during the build, 2026-08-25, at the owner's request.*

**New project is the header's trailing icon**, not a row in the list. It began
as a dashed card at the FOOT of the cards, which put the one thing you cannot
reach any other way behind however many boards you happen to have — and the
trailing glass circle is already this shell's "one action per section" slot
(Chats mints a conversation there). The empty state keeps its own full-width
CTA: on a screen with nothing else on it, a 45pt circle in the corner is not
where somebody looks.

**The list is ordered by what THIS PHONE opened last.** Purely local
(`ProjectRecency`, an `[id: ms]` map beside the board mirror), and deliberately
so: which board you reached for last is a fact about this device, not about the
account — the desk has its own order, the gateway stores nothing about a
client's attention, and a board opened on a laptop should not reorder the
phone's list. Logout deletes it with the mirror, because a project id that
meant `rglide` under one gateway means nothing under the next.

Two rules inside that:

- **A board never opened here keeps the server's order among its peers and
  follows the opened ones**, rather than sorting as if it were opened at the
  epoch. A `?? 0` sort would interleave unseen boards by an answer nobody gave.
- **The stamp is written in `openProjectBoard`**, the one door into a board —
  not in the card row's press. The cards root, the create flow and a push tap
  all come through that function, and a stamp on the row would miss the other
  two. It is also why a just-created board leads the list rather than sinking
  to the bottom a second after it was made: creating opens it.

### 9.1 Filing a card

The board's header trailing slot carries a `+` — the same "one action per
screen" idiom as the cards root's. It opens `NewIssueScreen` **in the column
the board was showing**, which is the web's rule (`CreateIssueModal`'s
`initialStatus`): filing from the Todo tab and finding the card in Backlog is
a small betrayal every time.

**Opening a card straight into In Progress starts a run.** `Transition::created`
dispatches exactly as a move into that column does, and `validate_staffing`
refuses it without an assignee — the same two facts the Move sheet already
states. So the rule lives in ONE place (`MoveConsequence.startingNote`) and
the two callers supply their own verb: a move "moves", a create "opens". The
other four columns say nothing at all, because a card that opens in Backlog
does nothing, and a sentence about a run that never existed is worse than
silence.

The form's Create button is off for a card the board would refuse, rather than
offering a press that can only 400.

**An archived board offers no `+` at all** — it takes no writes, so the slot
carries the `archived` chip explaining why instead of a button that can only
fail. The two are mutually exclusive by construction.

Not offered on this NEW-CARD screen: attachments (a card's files arrive on a
comment, from the dock — see §3.3 — or on its description from the web) and
`parent`/`stage` (filing a sub-card is a thing you do FROM the parent, and
there is no parent in view here).

### 9.2 Agents' faces

An agent draws its **uploaded avatar** (`TeamMemberInfo.avatar_blob_id`) when
it has one, and a monogram when it does not.

**iOS deliberately does not draw the generated face `app/web` shows.** The web
fills that gap with a Bottts robot seeded on the agent's profile id
(`components/botttsFace.ts`); DiceBear is not portable to Swift, and a
*different* generated face on each device would be worse than none — two
surfaces claiming to depict the same teammate with different pictures. The
monogram is honestly "there is no picture", and it is derived from the handle
printed beside it, so the two agree.

`AgentAvatars` is one store for the whole app, keyed by BLOB id:

- **Not a fetch per drawing.** The same teammate appears on every card it owns,
  in the board's face strip, in the assignee picker, in the filter sheet and on
  its own profile. `app/web`'s `useTeamPortraits` carries the same comment.
- **Keyed by blob, not agent.** Replacing an avatar mints a new blob, so a
  stale picture cannot survive under the agent's key — and two agents sharing
  one image cost one fetch.
- **A failure is remembered.** A blob that answered with nothing usable is not
  retried on every repaint; that agent falls back to the monogram like one that
  never had a picture.
- **Logout clears it**, because a blob id means nothing under the next gateway.

The board loads the whole roster's faces once when it arrives, rather than each
face loading its own — a face knows its blob and nothing about the others, so a
face-driven fetch is one fetch per drawing by construction.

### 9.3 A card's local cache

A card mirrors its own content (`issue-<project>-<number>.json`) so it paints
before the network answers — the board's rule, REPLACE and never merge, for
the board's reason: there is no local state here worth protecting, and a merge
would only invent ways for the two to disagree.

The timeline rides as its **raw envelope**, the bytes the gateway sent. Its
only consumer is the webview, and a Swift mirror of it would be a third place
every new event kind has to be taught about. The team rides along rather than
being read out of the board's mirror, because a card can be opened without its
board ever having been — a `#N` link inside another card's prose is a door
straight to it.

**What a mirrored card may NOT arm.** This is the part worth reading twice:

- **Parked approvals.** A prompt is a live queue entry with a 300s timeout, and
  one replayed off disk would ask for an answer to something that stopped
  listening hours ago. `ProjectsStore` refuses to cache prompts at all for
  exactly this reason; a card caches its timeline (the Activity has to paint)
  but withholds `pendingApprovals` until a live answer lands.
- **The live run, and therefore Stop.** A run unsettled when the mirror was
  written may have finished long ago, and the header's Stop would be offering
  to end something already over.

Both hang off one flag, `isFromMirror`, cleared by **this fetch's own answer**
— not by `self.issue != nil`, which is true the moment a mirror loads and would
arm the live controls off a card the network never confirmed.

Logout deletes every cached card with the board mirrors: one belongs to the
gateway that served it.

**The escape hatch.** The card's ⋯ carries *Rebuild this card* — the chat's
per-session resync, applied here. Deliberately not a new reconciliation
routine: a freshly installed device renders this card correctly off the same
server data, so the reconstruction known to be right is the one a first open
runs. Three steps — delete the mirror, drop what is in memory, reload the
document — and nothing else.

Step 2 is the one that differs from chat. There, the rows live in the webview
and the store holds almost nothing; here native holds the content and pushes
it, so clearing native state IS "a page with no memory".

A "reset yourself" bridge message is deliberately not what this is: it could
only clear the state somebody thought to list, and state that was not cleared
when it should have been is exactly what the hatch exists to escape.

**One chat scar that does not apply**: there is no `discardPersist` here,
because the card page never writes the mirror — native does, after a fetch.
The dying document has no persist to resurrect what step 1 just deleted.

It does not touch the board's own mirror (a card is not its board) or the live
approval queue (answering is REST, so a parked prompt survives untouched and
is re-derived from the refetched timeline). No confirm dialog: it throws away
a local copy only, and the page's own loading line is the feedback — chat
needs a banner because its rows live in the webview and there is nothing else
to look at meanwhile.

The on-disk shapes moved to `ProjectMirrors.swift` when this became the second
mirror needing them — a card, its runs and its team are the same records the
board already writes, and two copies would be two file formats kept identical
by hand. `IssueMirror` gained an `attachments` field there: the board never
drew a card's files, so it never wrote them.

### 9.7 The card page was wearing the wrong clothes

Found while adding the unread rule, and worth writing down because nothing in
four tiers of tests could see it: `issue.css` referenced `--ink`, `--ink-soft`,
`--paper`, `--surface`, `--line`, `--err` and `--mono` — **seven variables that
do not exist**. The bundle's tokens are `--color-ink` and `--font-mono`
(`styles.css:6`), and the page had been written against the shorter names.

Nothing failed. An unresolvable `var()` is *invalid at computed-value time*, so
each declaration quietly fell back:

- `border: 1px solid var(--line)` unsets the whole shorthand — `border-style`
  goes to `none`, so the chips, the section rules and the run list had **no
  border at all**, not a dark one;
- every `color: var(--ink-soft)` inherited full ink, so the greys the layout
  reads by — `@handle`, timestamps, the fold's "N events ›" — were as loud as
  the text;
- the one `var(--err)` inherited ink too, so a run's error printed black;
- the mono runs were the only harmless ones, and only by luck: `:root` sets
  `font-family: var(--font-mono)`, so falling back to inherit landed on the
  font they were asking for.

The result was a plausible page in the wrong clothes, which is why it survived
a whole build phase and several screenshots. Verified both directions in a
browser rather than by reading: `.issue-chip` computes `1px solid #e4e4e4`
now, and re-injecting the old declaration puts it back to `none / #111`. A
missing token is silent by construction — the only thing that catches one is
spelling it the way `styles.css` does.

### 9.6 One meaning for the red count

The tab badge, a card's badge and the board's strip all count the same thing:
**tool calls parked on an approval gate**.

The server's `/projects/attention` is wider — approvals + failed + unread — and
the badges deliberately no longer follow it. Two reasons:

- **A count you cannot discharge is a mark you learn to ignore.** A failed run
  and an unread card are not answerable; folding them into the same red number
  as a parked prompt makes the number mean "some things happened", which is not
  worth a badge.
- **The number you press and the rows you land on must agree.** A card reading
  `6` that opens onto a strip of one row is worse than either number alone.

The cost is that the phone's badge and the web's rail no longer show the same
figure. That is accepted: they are answering different questions, and the
phone's is the narrower, more useful one.

### 9.5 A Waiting row leaves on the press

Every board verb applies its effect locally before the write lands, and rolls
back on refusal — `write`'s snapshot. Two did not: `resolveApproval` and
`retryRun` passed no `apply` closure, so the row they answered sat unchanged
for a full round trip. It reads as a button that did nothing.

Both now predict what the server will say:

- **A retry clears `last_run_failed`.** The flag asks whether the NEWEST run
  failed (`FAILED_CARD_PREDICATE` in `crates/storage/src/sqlite/project.rs`),
  and a retry makes the newest one queued — so the prediction is exact.
- **An answer retires its prompt.** Prompts do not live on `Board`, so
  `write`'s snapshot cannot restore them; `resolveApproval` keeps its own. A
  404 ("already closed") deliberately does NOT restore — gone is gone — while
  any other refusal puts the row back, because the gate is still waiting and
  an operator left with an invisible prompt cannot answer it.

The live queue is still the truth. Being the truth is a reason to let the
refetch CORRECT this, not a reason to make somebody wait for it.

### 9.4 Hit targets

Three controls here needed an explicit `contentShape`, all the same bug: under
`.buttonStyle(.plain)` a label is tappable only where it PAINTS.

- **The card row** opened only where letters were. The gap after a short
  title, the space between the handle and the run word, the right margin and
  both vertical paddings were dead — a list that ignores half its taps.
- **The budget chip** is a stroke-only capsule: a 1px outline hit-tests a 1px
  outline, and the interior was dead. The logout pill shipped this exact bug
  once already.
- **Undo** reached for a 44pt target with `.frame(minHeight: 44)`, which is
  layout and nothing else. It is the one control here with a three-second
  life, so a missed tap on it cannot be tried again.

The cards root was already fine and was checked rather than assumed: a
`ProjectCardView` has a filled background, and a filled shape hit-tests.

## 10. What shipped, and what did not

Built across P0–P8: the gateway's `approval_pending` and archived-board guard,
the whole ffi surface, the Swift data layer and its pure models, the cards
root, the board, the card page (a third webview), run transcripts, and the
team / profile / activity / settings screens.

**Deferred deliberately, and worth naming so nobody looks for them:**

- **`@` mention chips on a comment.** Mentioning is typing `@handle`, and since
  2026-08-26 nothing on either client says what that will do in advance — an
  unassigned card is the only one where a mention does anything a plain comment
  does not.
  ~~Staged attachments~~ — **shipped 2026-08-25**, see §3.3. What made them a
  deferral was one `weak var store: ChatStore?` on the staging machine; that
  is a `ComposerHost` now, and the card has the same strip.
- **The description editor is a `<textarea>` over the raw markdown**, which is
  what §3.3 asks for — but it has had no device pass, and a Chinese keyboard is
  specifically what it needs (this app has one marked-text scar already).
- **The card page's pickers.** `pick(field)` crosses the bridge and the screen
  holds it; the sheets themselves are the board's, reached from the board.
- **Everything is simulator-only.** No tier here has run on hardware.

**The two questions §0.2 raised are still open**, and neither was P8's to
close: a run that reaches an approval gate just after its board is archived
still waits out the 300s timeout and self-denies, and `Frame::ResolveApproval`
is still a second door that checks no board.
