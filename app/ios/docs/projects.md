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
| 8 | Tab badge | **Native `.badge(n)`** on **both** Projects and Chats (a deliberate divergence from web's "a tab gets a dot, never a count") | §3.0 |
| 9 | Mark-read by swiping a Waiting-on-you row | **No** | §3.1 |
| 10 | Confirm before Stop | **Yes, a ConfirmDialog** | §3.3 |
| 11 | Editing the description | **A ✎ button; raw text while editing, rendered again on exit** — in v1 | §3.3 |

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
      ├─ header: [←] project name … [+ new issue]
      ├─ stage segmented control (5 segments, tap to switch; a segment with news wears a red dot;
      │   the bar strip itself takes a horizontal swipe to change stage)
      ├─ board row: team faces (working = ring) · budget chip (only over ceiling) · filter · ⋯
      ├─ "Waiting on you" strip (current board only, only when non-empty, collapsible):
      │    approval (inline Deny/Approve) · failed run (Run again) · agent question (Answer) · unread card (open it)
      └─ the open stage's wall of cards: bands Pinned / New / Queue
           ├─ tap a card → Issue detail (push)
           │    ├─ body = issue.html webview (title · chips · live-run · description · attachments ·
           │    │   sub-issues · runs · activity)
           │    ├─ a run row → Run transcript sheet (transcript webview, read-only)
           │    ├─ a chip → native pickers (status / priority / assignee / parent+stage)
           │    ├─ ⋯ → Rename · Pin · Block…/Unblock · Cancel/Reopen · Copy branch
           │    ├─ sub-issue / #N → child card (push; back returns to the card you came from)
           │    └─ dock (native): hint chip · @ chips · ApprovalCardView (two answers) · composer pill
           ├─ long press → Move to… · Assign… · Pin · Block… · Cancel issue
           ├─ leading swipe → Pin · trailing swipe → Move… (full-swipe disabled)
           ├─ ⋯ → Activity (push) · Team (sheet → Agent profile) · Mark all read N · Settings (push)
           └─ + → New issue sheet (document-first)
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

- **One stage at a time** — the web `ColumnPage` on a phone. The segmented control carries live counts (**cancelled excluded**); a segment with unread wears a red dot; the `Pinned / New / Queue` bands only print a header when more than one is non-empty. **No `TabView(.page)`**: a row's swipe actions, page paging, and the edge-back gesture are three horizontal gestures fighting. The bar strip (segmented + board row + Waiting strip) takes the horizontal swipe instead.
- **Reading order = pinned → unread → `position`**, rendered only, never written back; a cancelled card is never lifted by unread (a pin does lift it); a refresh anchors scroll by row id.
- **A card is a row** (`ChatRowBody` grammar, `Theme.sys`): a 3px spine on the leading edge (urgent/high ink, medium light grey); first line `#N · ▲▲/▲/◆/▽ priority (all ink — red is only for failure) · (pin) · age · hand glyph (approval_pending) · red unread count`; two lines of title; a badge row of Blocked / ✕ Run failed (the one red thing) / ⑂ branch (only once it has a commit) / ↳ #N; a footer of assignee face + handle, **the runner's face** (a second, ringed face when the run is not the assignee's — a coordination run is @lead's), the run word `WORKING · 4m` / `QUEUED` / `HELD · 41m` (running measures from `started_at_ms`, queued/held from `created_at_ms`, nothing after it settles), and the `done/total` progress ring. A cancelled card is struck through and dimmed but still opens (Reopen).
- **The "Waiting on you" strip**: the current board only, compact rows rather than whole cards, four kinds — an approval (Deny / Approve inline; found by taking the `approval_pending` cards and reading their `events` for the `call_id`, which is bounded), a failed run (Run again), **an agent's question** (`blocked_reason` set and the newest `blocked` event's actor is an agent → "@lead asks on #7: …" + Answer), and an unread card (opening is the only way to clear it — decision 9). No countdown (the 300s timeout is a gateway-private constant); answering a prompt that is gone returns 404 → the row becomes "Closed — timed out or already answered" and refreshes.
- **The board row**: team faces → budget chip (`⏸ $6.10 / $5.00`, only when `burnState == over`, opens Settings; **a standing condition, not news, so it never feeds a red dot**) → filter chip (ink-filled with a count when narrowed) → ⋯ (Activity · Team · **Mark all read N** · Settings).
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
| header | native (`ChatHeaderView` grammar) | back · `#N · status` glass pill (tap → Move sheet) · ⋯ |
| body | **`issue.html` WKWebView** | title · chips · live-run row · blocked banner · ↳ / ⑂ · description (full markdown / KaTeX / images) · attachments · sub-issues · runs · activity (comments as markdown; **consecutive system events collapse into "N events ›"**, while comments, approvals and blocks never collapse) · a "New activity" jump pill |
| dock | native (`ComposerDock` grammar) | hint chip · @ chips · `ApprovalCardView` (two answers, REST-backed) · composer pill (+ attachment bloom) |
| overlays | native | pickers (`ModelMenuPanel` style) · sheets · `RenameDialog` · `ConfirmDialog` |

- Why the whole body rather than a small webview for the description alone: a webview inside a native ScrollView is two scrollers plus height round-trips, while a full-page webview is exactly `ChatScreen`'s existing layering (header / webview / dock / bottom-inset stream) — and comment markdown, attachments, `#N` links and KaTeX all come free.
- Shape: a third Vite entry `issue.html`; an `IssueBridge` (the size of `DeckBridge`, with its main-frame-only guard and 3-reloads-per-30s crash budget): native→web `init / deliverIssue / deliverEvents / deliverRuns / setBottomInset / jumpToLatest / blobResult / editDescription`, web→native `ready / pick(field) / openRun / openIssue(n) / requestBlob / viewImage / previewFile / copy / log / descriptionDone`. **Inline images ride the bridge's `requestBlob`/`blobResult` (as the transcript does), so no scheme route is needed**; putting HtmlPreview in the issue body would need `DynamicRoute` widened, which v1 skips. The keyboard: the webview never resizes; native streams the bottom inset (the transcript's mechanism). One host per card, torn down on exit.
- The live-run row: running → Stop → **`ConfirmDialog`** (decision 10) "Stop run #k? The card stays where it is. Stopping is the only way to end a run."; held → `@h is held — over the daily token ceiling` + **Run it again** (on a held card the press is what releases it, so it is never greyed out); failed → `✕ Run #k failed — <error>` + Run again. Stop lives only here and in the transcript header, never in a long-press menu.
- **The hint chip** (native): the **third mirror** of `comments::comment_delivery` (`crates/project/src/comments.rs:37`) and `mentions::assigns_to`; the web's two live in `timelineModel.ts:252` and `mentionModel.ts`. Wording is taken from the web verbatim. The rule is not exposed over REST, so every client re-derives it — which is why it **must** be pinned by golden fixtures shared with app/web (§7).
- **The approval card**: `ApprovalCardView` unchanged in the dock (`CompactPillButtonStyle` lifted out of its file first), two answers; the pending set is the card's `events` replayed by `call_id` (requested without resolved). The live queue is the truth, so tolerate a 404 on answer.
- **The Answer flow** (an agent's question): Answer from the Waiting strip or the blocked banner opens the card with the composer focused and `@lead ` prefilled, the hint reading "Answers @lead · unblocking hands the run back to @dev-2", and "Unblock #N after sending" checked by default → `POST comment` first, then `PATCH {blocked_reason: null}` (the unblock door hands the parked run back out, and its brief carries your answer). A block the operator wrote themselves does not get this treatment.
- **Editing the description (decision 11)**: a ✎ in the Description section header swaps the rendered block for a plain `<textarea>` holding the raw markdown (deliberately not contenteditable), and the native dock becomes "Editing description · Cancel | Done". Done sends the text over the bridge → `PATCH {description}` → re-render. Renaming the title still goes through `RenameDialog` (⋯ → Rename).
- `POST …/read` fires only after the timeline renders successfully, then attention is refetched.

### 3.4 Run transcript (sheet)

- Reuses the transcript webview through `ProjectRunReadStore: TranscriptTarget` (a `SubagentReadStore` twin: read-only by type, **`mirrored = false` kept** — decision 5's mirror is the board snapshot, never a run transcript).
- Data: `GET …/runs/{attempt}/transcript` (a `ChatSessionDetail`, paged by `before_ordinal`/`limit`). **There is no sync endpoint**: while a run is unsettled the page advances by re-reading the newest history page on `ProjectChanged{scope: run|timeline}`, degrading to a 2s poll if frames are unavailable, with one last read after it settles. The FFI must split `history_page` into a `history_page_at(full_path)` (today's implementation hardcodes `{base}/{id}`).
- It is one agent's session on that card (an attempt's page therefore also holds the attempts before it; the brief is a user bubble, which segments it naturally — per-run headers are v2). Read-only: no composer, no approve/deny (answer on the card); the header carries Stop (with the confirm).

### 3.5 The rest

- **Team sheet / Agent profile**: faces (working ringed, queued dimmed, lead heavier), handle, role, state `on #12 / queued #15 / idle`. The profile adds: state line · Runs on (fixed at hire) · Model pin (`PUT /v1/agents/{id}/model`) · Joined · On its plate · Remove (hidden for the lead; a busy agent shows "Busy on #12 — can be removed once idle"). **Hiring is v2.**
- **Activity (pushed)**: a read-only stream; a row is a glyph, one sentence (actor / #N / the word that ends a run in ink) and a relative time; failure is the only red; tapping opens the card. Unknown timeline/feed kinds must render through a fallback — master is still adding kinds.
- **Filter sheet**: search (title or #N) · assignee · **Running only** · Unread / Blocked / Failed / Held only · Hide cancelled · Clear. The filter chip carries the count of active narrowings.
- **Project settings (pushed)**: name / description / both ceilings (with today's spend and the web's hint sentences) / parallel runs / workdir (read-only) / Archive (`ConfirmDialog`; unarchive asks nothing). **`PUT` is a full replace** — an omitted field clears it. The over-ceiling chip deep-links here and highlights the ceiling that is biting.
- **New issue sheet**: document-first; chips prefilled from the open stage; `⚡ In Progress + @h — creating this starts a run`; In Progress without an assignee cannot submit.
- **New project (pushed)**: Name (required; the server creates `work/<name>`) · Description · optional ceilings and parallel runs (the web's hints) → on success, push the new board (its lead is hired with it).
- **Empty state**: zero projects → icon + a line that also states boards are never pushed + New project.
- **An archived board**: an `Archived · read only` chip; + greyed; move / comment / assign / ✎ disabled and carrying the server's 409 sentence; Mark read, Stop and Unarchive still work; **approvals are not answerable** (the card renders greyed with "Archived — unarchive to answer" and the dock hosts no approval card; `/attention` already excludes archived boards, so nothing leads you to an unanswerable prompt).
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
| An @mention on a staffed card is a question, not a reassignment; on a blocked card it staffs nobody | the hint chip mirrors it |
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
- **Golden fixtures shared with app/web**, copying the whole `searchSnippetVectors.json` precedent: `app/web/src/pages/projects/commentHintVectors.json` generated by an mjs script calling the web's real `commentHint`, imported directly by web vitest, and read off disk by a Swift test that walks up from `#filePath` to the repo root (as `SearchSnippetVectorTests.swift` does, coverage canary included). `approvalReplayVectors.json` the same way. Three mirrors (`comments.rs` ↔ `timelineModel.ts` ↔ Swift) pinned by one set of vectors.
- A `-baybo-demo-projects` fixture with no gateway (hung off the `-baybo-open-home` block, short-circuited by `demoHomeMode`); `-baybo-home-tab projects` already exists.
- Accessibility: rows carry label = title, value = subtitle; any bar whose children come and go gets `.accessibilityElement(children: .contain)`; the Waiting strip's buttons need `.buttonStyle(.plain)` isolation; floating panels need pixel sampling (UI tests are blind to paint).
- New web files start with zero eslint suppressions; extracting the attachment components out of `Transcript.tsx` shifts suppression counts between files, so the baseline gets regenerated.

## 8. Scope

- **v1**: cards root; board (one stage + the four-kind Waiting strip + bands + pull-to-refresh + skeleton + run elapsed + Undo toast + stage-bar swipe + haptics); Move / long press / swipe; Issue detail (webview body + native dock: two-answer approvals, Stop with confirm, Run again, Answer+unblock, comments + hint + @mention + attachments, ✎ description editing, collapsed activity); run transcript sheet; Team + profile (read / model pin / remove); Activity; Filter (with Running only); Settings (with archive); New issue; New project; empty / archived / offline states; the mirror; tab badges.
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

Not offered on this screen: attachments (they want the composer's staging
strip, which is bound to `ChatStore`; a card takes files from its own page)
and `parent`/`stage` (filing a sub-card is a thing you do FROM the parent, and
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

## 10. What shipped, and what did not

Built across P0–P8: the gateway's `approval_pending` and archived-board guard,
the whole ffi surface, the Swift data layer and its pure models, the cards
root, the board, the card page (a third webview), run transcripts, and the
team / profile / activity / settings screens.

**Deferred deliberately, and worth naming so nobody looks for them:**

- **`@` mention chips and staged attachments on a comment.** The card's dock
  takes text. Mentioning is typing `@handle`, which the hint line already
  describes the effect of; attaching would want the composer's whole staging
  strip, which is bound to `ChatStore` from its initialiser down.
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
