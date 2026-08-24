# iOS Projects tab — implementation plan

*Written 2026-08-24 against HEAD 306b22d7 (kanban merged into master; feat/ios-swiftui fast-forwarded onto it). Design: [projects.md](projects.md). One PR per phase through `scripts/dev-merge-sync.sh` (opens as a draft; the owner runs `gh pr ready`; the script then verifies each of the five REQUIRED_CHECKS reports `pass` rather than `skipping`). **All three iOS CI jobs are `if: false`** (ios-web only because the Actions quota ran out, and it is the cheapest to restore) — every iOS-side phase runs its suites on a laptop and says so in the PR body.*

Anchors: `P:` = `crates/gateway/src/api/admin/projects.rs`, `pump:` = `app/ios/ffi/src/transport/pump.rs`, `GA:` = `app/ios/ffi/src/gateway_api.rs`.

## P0 · Gateway catch-up — **done** (shipped with this document)

The only server work standing in front of the iOS client. What landed, against the sketch below: `approval_pending` is resolved by `cards_awaiting_approval` (projects.rs, beside the `on_board` helper) off one owner-channel snapshot, short-circuiting when the gateway has no parked prompt at all — so the ordinary response pays neither a project read nor a session read. It is `false` on an archived board, which is where the archived rule and the badge stay one answer. `resolve_approval` now goes through `ProjectManager::approvable_issue` (`writable_project` + `get_issue`) and 409s an archived board. `ProjectChangeScope::Unknown` carries `#[serde(other)]`, with a test that a frame naming a scope this build has never heard of still decodes. The web mirror in `chatWs.ts` widened with it, and 13 app/web fixtures gained the new required field.

1. **`IssueDto.approval_pending: bool`** (decision 4)
   - Add the field to `IssueDto` (around P:314), serialized like `last_run_failed` (P:368); the `From<IssueRow>` fallback keeps `false`.
   - Give `IssueDto::on_board` (P:399) a `pending: &HashSet<IssueId>` parameter. A private helper next to `parked_approval_session` (P:1629) snapshots `pending_approval_sessions()` off the owner channel (no `.await` between the snapshot and its use — the discipline comment at P:1847-1849), maps each session through `session_manager.get(sid)` → `trigger.issue()`, filters to this board and collects the issue ids. Wire it into all five construction sites: the `on_board` helper (P:65, which covers create/get/update/move) and the `list_issues` loop (P:1324).
   - The session→issue mapping reuses the spelling `resolve_approval` already trusts (P:1675-1690) rather than inventing a third. If the per-pending session read is judged too chatty, the alternative is a store-side `issues_for_sessions` (the `projects_for_sessions` SQL plus the `number` column, `storage/sqlite/project.rs:1135`) — pick one deliberately.
2. **Refuse approvals on an archived board** (decision 3; there is no guard today, so an archived board's prompt is answerable while `attention` already refuses to count it — exactly the drift this closes)
   - One home for the rule: add a small `ProjectManager` verb, `approvable_issue(id, number)` = `writable_project` (`crates/project/src/manager.rs:3068`) + `get_issue`, and use it in the handler in place of P:1666. `ProjectError::Archived` already maps to 409 (P:59). Add the 409 to the utoipa `responses` block (P:1650-1655).
   - Test after `an_archived_project_leaves_the_listing_and_stops_taking_writes` (`crates/gateway/tests/projects_api.rs:421`) plus the `park_approval` fixture (:1366).
3. **An unknown-variant fallback for `ProjectChangeScope`**: the wire enum (`crates/wire/src/lib.rs:106-118`) has no `#[serde(other)]` arm, so a gateway that adds a scope breaks whole-frame decoding on an older phone. Add an `Unknown` variant (the same tolerance argument as `ChatSubagentStatus::Unknown`). **Touching wire means `scripts/check-ts-bindings.sh` surfaces regenerate** (`sidecars/sdk/channel-ts/src/generated`).
4. Regenerate: `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync`, then `pnpm --filter baybo-web gen:api`. Commit `schema.d.ts` with `docs/openapi.json` in the same PR, or `app/ios/web`'s type-only `restSentinel` import sees a stale file.
   - Optional, non-blocking: let the web board consume `approval_pending`.
- Gates: root `cargo fmt` / `clippy --all --benches --tests --examples --all-features` / `nextest run --workspace` (**without** `--all-features`), the openapi sync test, and the ts-bindings script.

## P1 · The FFI surface — **done**

25 exported calls, the `ProjectSink` lane and the `StringPatch` tri-state, against the sketch below. Three things landed differently, each for a reason worth keeping:

- **`project_update` returns nothing.** The route answers with the row, but a full replace is a body the caller authored field by field — there is nothing in the response it does not already hold — so it rides `put_empty` rather than growing the trait a `put_json` for one call site.
- **The timeline, the feed and a run's transcript answer raw gateway JSON**, not records. Their consumer is the issue webview, which renders the gateway's own shape; mirroring a 20-arm tagged union through UniFFI would buy a second shape to keep in step and nothing else. The pieces the native side needs off a timeline (a parked prompt's `call_id`, who blocked the card) are one small model's job in P2.
- **`agent_set_model` deferred to P7.** It is an `/v1/agents` route, not a board one, and its only caller is the agent profile that phase builds.

Every enum decodes through an `Unknown` arm and refuses to *encode* one — a card whose gateway grew a status costs that card its word rather than blanking the board, while asking the server to move a card into a column this build cannot name fails before the request. `Frame::ProjectChanged` now has a consuming pump lane placed before the catch-all; a session-less `Gap` nudges the project sink too, since a board has no other way to learn it missed an invalidation.

1. **A PATCH verb**: add `patch_json` to the `GatewayJsonClient` trait (GA:29-77) and to all four implementations — `DirectHttp` (`direct/mod.rs:171`, reqwest `.patch()`), relay (`relay/api.rs:53`, where the method is a `&str`: `request("PATCH", …)`; `ReplayPolicy::Converges` holds for absolute-value PATCHes), the `ActiveGatewayClient` `forward!` dispatcher (`gateway_client.rs:30-65`), and the `RecordingClient` test double (GA ~1360).
2. **Reads** (the tolerant wire-struct → record pattern of `SessionSummary`, GA:128-163): a `PATH_PROJECTS` const, then `project_list(include_archived)`, `project_get`, `project_issues` (mind the Done paging parameters), `project_active_runs`, `project_team`, `projects_attention`, `projects_activity(since_ms)`, `project_issue_get`, `project_issue_events`, `project_issue_runs`, `project_feed(before_ms)`.
3. **Transcript**: split the private `history_page` (GA:969-994, which hardcodes `{base}/{id}`) into a `history_page_at(full_path)`, then add `project_fetch_run_history(p, n, attempt, before_ordinal, limit)` with `validate_path_segment` on all three segments. **There is no sync endpoint** — do not build a sync twin.
4. **Writes**: `project_issue_create`; `project_issue_patch` with a **`StringPatch { Keep, Clear, Set{value} }`** uniffi enum serialized to `Option<Option<&str>>` (the gateway's `double_option` is ready at P:1105-1137; UniFFI cannot express `Option<Option<T>>`, which is what forces the enum); `project_issue_move(status, ordered_numbers)`; `project_issue_comment(text, attachments)`; `project_issue_read`; `project_read`; `project_run_cancel`; `project_run_retry`; `project_approval_resolve(p, n, call_id, decision)` (a two-answer enum); `project_create`; `project_update` (its doc comment must state the PUT full-replace semantics); `project_archive`; `project_remove_agent`; `agent_set_model`.
5. **ProjectSink**: the trait (api.rs, `DeckSink` pattern at :683-692, `with_foreign`; the Swift implementation must not be named `*Impl`) → `set_project_sink` (lib.rs:146-149, set on both legs) → `transport/mod.rs` (`SharedProjectSink` :195-201, registry field :211-223, init, setter :377-385, free fn :480-487) → `supervisor.rs` (spawn params :874-883, `PumpCtx` construction :667-670) → **a consuming arm in pump.rs placed before the `_ => true` catch-all** (the `DeckChanged` arm at :283-286, returning `false`). Today `ProjectChanged` falls through the catch-all and is broadcast into every transcript sink, so the arm must consume it. `Gap{None}` should also nudge the ProjectSink (or the board store must subscribe to the list-stale signal), or an invalidation dropped by the gateway's bounded broadcast queue is lost silently.
6. `FakeBayboClient` (`Tests/Support/FakeBayboClient.swift:13`) gains every new method (conformance breaks at compile time, which is the point); `transport/tests.rs` gets a `ProjectChanged` routing test modelled on the deck arm tests (:363-405); regenerate bindings with `scripts/build-core.sh` — but **its default run deletes the signed device xcframework**, so Swift/web loops use `build-app.sh --skip-rust`.
- Gates: from `app/ios/`, `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo nextest run --workspace`. Compiling BayboTests is the only thing in the repo that catches UniFFI seam drift.

## P2 · Swift data layer, pure models, fixtures — **done**

Six pure models (`BoardOrder`, `RunLabels`, `MoveConsequence`, `CommentHint`, `IssueTimeline`, `BudgetMeter`), `ProjectsStore` with its mirror and `ProjectEventsRelay`, and the shared golden vectors. Notes against the sketch below:

- **The mirror carries its own structs.** UniFFI generates `Equatable`/`Hashable` but not `Codable`, so the on-disk shape is written out by hand — which is the right answer anyway: a mirror is a file format an upgrade has to keep reading, and pinning it to the transport's shape means a gateway field that moves invalidates a mirror the user already has. A run's cost is deliberately never mirrored (`nil` is not zero, and the active-run poll does not price runs).
- **`IssueApprovalPrompt`, not `PendingApproval`.** The chat surface already owns that name, and the two are genuinely different planes: a chat prompt is derived from a subscribed session's frames and answered over the WS; a board prompt is read off a card's timeline and answered by `call_id` over REST.
- **`-baybo-demo-projects` moved to P3.** A demo fixture's only observable effect is on screen, and there is no Projects screen yet — writing it here would be code no test could assert.
- The cross-end gate is `app/web/src/pages/projects/commentHintVectors.json`: 16 comment + 10 mention vectors, generated from the web's own `commentHint`/`mentionHint`, asserted by `commentHintVectors.test.ts` (web) and `CommentHintVectorTests` (Swift) over the same file. Regenerate with `pnpm --filter baybo-web gen:comment-hint-vectors`; the Swift suite going red after a regen is the gate working.

### Original sketch

1. **ProjectsStore** (`App/Core/`): DeckStore's REPLACE plus SessionIndex's injected support directory (so parallel Swift Testing suites stay isolated — `TempSupportDir`). Mirrors `projects.json` and `board-<id>.json`; reads on init so the first frame paints; `refreshNow()` replaces wholesale and persists; `removeMirror()` hangs off `AppStore.resetChatStores` (:1433); a lazy `clientProvider` (DeckStore :191-216) so constructing it in a test never boots the FFI; a failed write rolls back to the mirror (there is no outbox). Done keeps only its first page plus the count.
2. **ProjectEventsRelay** (the `DeckEventsRelay` shape at :909-933, main-actor hop, not named `*Impl`), registered at launch beside AppStore:300-303; 300ms debounce; any scope refreshes the open board; refreshes are held while a swipe panel is open.
3. **Pure models** (`App/Core/`, directly unit-tested): `BoardOrder`, `MoveConsequence`, `CommentHint`, `PendingApprovals` (replay by `call_id`), `BudgetMeter`, `RunLabels` (including elapsed).
4. **Golden fixtures** (the whole `searchSnippetVectors.json` chain): add `gen-comment-hint-vectors.mjs` to app/web calling the real `commentHint` (`timelineModel.ts:252`) to produce `commentHintVectors.json`; web vitest imports it; `Tests/CommentHintVectorTests.swift` reads it off disk by walking up from `#filePath`, with a coverage canary. `approvalReplayVectors.json` follows the same shape. This PR touches app/web (generator + test) — mind that project's eslint suppression baseline.
5. **Demo fixture**: `-baybo-demo-projects` inside the `-baybo-open-home` block (AppStore:348-459; `demoHomeMode` short-circuits the network, as `demoCronJobs` :914-961 does).
- Gates: Swift unit tests (`xcodegen` → `build-for-testing` + `test-without-building`; **never** a bare `xcodebuild test`) and app/web `pnpm lint` / `pnpm test`.

## P3 · Cards root, navigation, badges — **done**

1. Replace the `PlaceholderScreen` at `HomeScreen.swift:89-90` with the Projects cards root (the `section{}` wrapper supplies the wordmark header); empty state and New project.
2. Routes: add `.projectBoard(String)` and `.projectIssue(String, Int64)` to `AppStore.ChatRoute` (Hashable with payloads, as `.cronGroup` is; **keep transient state off the route** — the comment at AppStore:124-134); add two arms to the `RootView.swift:33-50` switch (the surrounding Group hides nav chrome for free); push with the guard-then-append shape of `openArchived` (AppStore:731-734) rather than `activateSession`, which switches tabs and resets the path; attach `PopGestureEnabler().frame(width: 0, height: 0)` to every pushed screen. The board→issue two-deep stack exercises PopGesture's peer-inheritance path (`PopGesture.swift:158-193`), which already exists.
3. **Tab badges**: `Tab(...).badge(n)` chained inside the existing ForEach (`HomeScreen.swift:23-33`). This is the repo's first `.badge` — **verify on a simulator before building on it**. Chats = `BadgeCenter.total(index.rows)` (:53-55); Projects = the attention sum. `HomeTabView` starts observing `SessionIndex` and the ProjectsStore.
4. The New project form (the `DirectLoginView` shape); on success, push the new board.
5. Strings: hand-edit `Localizable.xcstrings` with both `en` and `zh-Hans` units (`home.tab.projects` already exists).
- UI tests: a smoke test for the cards root (on a **fresh simulator** — the local iPhone 17 Pro is paired, which merges demo rows away).

**What the simulator changed.** Three things the plan assumed did not survive
contact:

- `.badge` renders, but SwiftUI exposes it to accessibility **nowhere** — the
  tab item's label stays the bare section name and the badge has no child
  element (dumped from the live tree). A test reading `label` would have passed
  a build that drew no badge at all, so `ProjectsUITests` asserts the disc in
  pixels via a new `ScreenshotPixels.redCoverage(in:)`, with Deck as the
  no-badge control.
- `AppStore.projectsStore` is a nested `ObservableObject`, so its changes do not
  republish `AppStore`; the badge read through `store` would have frozen at
  whatever it saw on first paint. `HomeTabView` now subscribes to
  `$attention` directly. The demo fixture cannot catch this — it seeds before
  the first render.
- The team faces on a card were an overlapping avatar stack with the working
  mark drawn as a ring around each face. On a four-agent board the rings crossed
  their neighbours, and the LEAD's ring vanished under its own heavier border —
  so the agent most likely to be working was the one face that could never say
  so. Faces are now set apart, the working mark is a corner dot, and monograms
  are made unique across the row (`dev-1` and `docs-1` both reduce to `D1`).


## P4 · The board screen — **done**

- The segmented control (live counts excluding cancelled, news dots) plus the horizontal swipe on the bar strip only (segmented / board row / Waiting strip, never the card rows); the board row (face ring, budget chip, filter chip, ⋯ menu); bands and reading order (render-only, scroll anchored by row id); the card row (`ChatRowBody` grammar: hand glyph, run word with elapsed, the two faces when a runner differs, spine, ring).
- The Waiting-on-you strip's four kinds (approval with inline two answers, failed with Run again, agent question with Answer, unread that only opens); `call_id` lookups only for cards flagged `approval_pending`.
- The Move sheet (its sentences come from `MoveConsequence`), the long-press menu, leading Pin / trailing Move (full-swipe disabled), the Undo toast for moves that start no run (the `ChatListScreen` undo machinery at :16-18 and :312-361), and the assignee-picker path.
- The Filter sheet (with Running only); the ⋯ menu (Activity / Team / Mark all read / Settings); pull-to-refresh (the hand-rolled mechanics at :150-170 plus `RefreshRing`); skeletons; offline write-disabling; archived read-only.
- UI tests: the Move sheet's consequence copy, `.buttonStyle(.plain)` isolation on the Waiting strip's buttons, pixel sampling for floating panels.

**What the simulator changed.**

- **The Waiting strip's answer buttons were not in the accessibility tree at
  all.** The row carries a tap gesture and an identifier, and SwiftUI folds
  such a container into one element — so Deny and Approve drew, and worked
  under a finger, and existed for nothing that reads the tree, VoiceOver
  included. Both the strip and each row now say `.accessibilityElement(children:
  .contain)`. The UI test found this; no amount of looking at the screen would
  have.
- **The monogram rule had to leave the view.** It lived inside `TeamFaces`, so
  the assignee picker and the filter sheet went on printing `D1` for both
  `@dev-1` and `@docs-1`. It is now `AgentMonogram`, which is a property of a
  SET of handles, and all three lists ask it.
- **Its cap was on the wrong quantity** — the first segment's width rather than
  the glyph count — so a dashed handle could reach four glyphs (`REV1`) in a
  circle that holds three. Caught by the test that asserted the ceiling.
- **The answer pills were sized to the 44pt hit floor**, which made "Deny" a
  disc inside a compact row. The painted capsule and the tappable area are now
  different sizes: the target never shrinks, only the paint does.


## P5 · Issue detail — **done** (split into a web PR and a native PR)

- **Web half**: add the `issue` entry to `vite.config.ts:17-22` and an `issue.html` (viewport meta copied from deck.html); a new `src/issue/` (new files start with zero eslint suppressions); **extract the attachment components out of `Transcript.tsx` into `src/attachments.tsx`** (AttachmentImage/Bubble/Video/Audio/File, `useNearViewport`, …) — this shifts the suppression baseline, so do it as its own commit; reuse `Markdown.tsx` directly (which pulls in `bridge.ts`: the cheapest resolution is to register the `baybo` handler on the issue webview too so `openUrl` works, otherwise parameterize it); repeat the KaTeX css and fontsource imports in the entry; an `issueSentinel.ts` pinning the hand-written DTO mirrors through `restSentinel`'s type-only import; `issue.*` strings in both locales with the parity test; collapsed activity (consecutive system events → "N events ›").
- **Native half**: `IssueBridge` (the DeckBridge pattern, main-frame guard, crash budget; `requestBlob`/`blobResult` as the transcript does) and `IssueHost` (if it adopts the transcript's `permitsNavigation`, that gate must admit `/issue.html`; deck's ungated delegate is the alternative); the bottom-inset stream (`setComposerTop` → `pushBottomInset`, replayed on `ready`); the dock (composer pill, hint chip from `CommentHint`, @ chips, `ApprovalCardView` with `CompactPillButtonStyle` lifted out first, the Unblock-after-send toggle, the Editing mode bar); the Stop `ConfirmDialog` (a snapshot struct like `PendingCronJobDelete`, hosted on RootView); native pickers; the ⋯ menu; `RenameDialog` for the title.
- `POST read` fires after the card renders, then attention refetches.
- Gates: `app/ios/web` `pnpm lint && pnpm test && pnpm build` (the build is the only evaluator of both drift sentinels), the two-step Swift test run, and a device pass — put the description editor through a Chinese keyboard.

**What the build and the simulator changed.**

- **`issueSentinel.ts` caught three wrong mirrors before any Swift existed.**
  Written by hand from the Rust source, `IssueDetail.assignee` was an
  `AgentRef` (it is a bare id), `IssueRun` carried an `agent` object (it
  carries `agent_id`), and `Actor` was externally tagged (it is internally
  tagged by `kind`). All three fail silently at runtime as a missing
  `@handle`; the build caught every one.
- **`TranscriptMedia` needed a seam, not a fork.** It was tied to the concrete
  `TranscriptBridge` by exactly one thing — the four replies it makes — so
  `WebMediaSink` narrows that to a protocol and `WebMediaDispatch` handles the
  inbound half once for both pages. A file card on a card behaves as it does in
  a conversation because it IS the same code on both ends.
- **The timeline is spliced, not re-encoded.** Its only consumer is the page, so
  it crosses as the gateway's own bytes; a Swift mirror of it would be a third
  place every new event kind has to be taught about. The card and its runs DO go
  through `IssueWire`, which is a mirror and says so.
- **A missing string key is invisible.** `lang.t` echoes the key on a miss, so
  the Stop dialog shipped a button labelled `chat.cancel` and every assertion
  stayed green. `LocalizedKeyTests` now walks every `lang.t("…")` in `App/` and
  fails on a key the catalog does not carry — and on one language carrying a key
  the other does not.
- **`App/Resources/transcript/` is a COMMITTED copy of `web/dist`.** `pnpm build`
  alone does not reach the app; the card page rendered blank until the copy step
  ran. `build-app.sh` does it, and a Swift/web loop that skips it is testing the
  last bundle.

**Deferred from P5, deliberately:** the `@` mention chips, staged attachments on
a comment, and the run-transcript sheet the card's run rows point at (P6). The
`onOpenRun` hook is wired and inert rather than absent, so the card's shape is
the one it keeps.


## P6 · Run transcript sheet

- `ProjectRunReadStore: TranscriptTarget` (`SubagentReadStore.swift:14-28`: `connEpoch 0`, `listed false`, **`mirrored false`**); data through P1's `project_fetch_run_history`; a live page re-reads the newest page on `ProjectChanged{run|timeline}` (the sink notification threading into the store), degrading to a 2s poll when frames are unavailable, with one read after it settles.
- The sheet shell is the `SubagentSheet` grammar (its own NavigationStack, `.large`); the header carries the run row and Stop (with the confirm).

## P7 · Team / profile / Activity / Settings

- Team sheet and Agent profile (model pin through `agent_set_model`; Remove hidden for the lead, explained for a busy agent); the Activity push screen (unknown kinds fall back rather than crash); Project settings (the PUT full replace means the form must send every field back; Archive behind a `ConfirmDialog`).

## P8 · Wrap-up

- Fill out the four test tiers. Docs: fix `navigation.md:21` ("projects is `PlaceholderScreen`") and add Projects to the real-screens list plus the badge description; add a cross-reference paragraph to `approvals.md` distinguishing board approvals (REST, two answers) from chat approvals; note in `chat-list.md` that Projects deliberately stays out of the app-icon badge; register the `-baybo-demo-projects` fixture and the golden-fixture home in `testing.md`; document the ProjectSink lane in `connection.md` and the ffi docs; archive this plan.
- Odds and ends: add a `Haptics` warning variant if the touch vocabulary needs one (only `tap` and `success` exist); consider restoring the `ios-web` CI job — it was switched off for quota alone and is the only CI evaluator of the two sentinels.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `Tab.badge` may not render in that ForEach shape (no precedent in the repo) | A ten-minute spike on day one of P3; if it fails, fall back to counts on the cards root only (no information is lost) |
| Extracting the attachment components shifts the eslint suppression baseline | Do the extraction and the baseline regeneration in their own commit, separate from behaviour |
| `build-core.sh` clobbers the signed device xcframework | Swift/web loops always use `build-app.sh --skip-rust`; re-sign by running `build-core.sh` from an interactive GUI terminal |
| ~20-31 nextest failures on this machine are environmental (no ripgrep, nested sandbox-exec) | Compare against the master baseline; do not chase environment noise — CI is the authority |
| UI tests destroy a paired simulator | Always a throwaway `simctl` device |
| master keeps adding project timeline/feed kinds | Activity and timeline render unknown kinds through a fallback |
| The 300s approval timeout is short for a phone | v1 accepts it (404 → "already closed" copy); raise it with the gateway during the push phase |
