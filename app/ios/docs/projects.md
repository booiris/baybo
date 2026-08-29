# iOS Projects

The Projects tab is the phone client for Baybo boards. Board rules remain in
`baybo-project`; the app renders server-resolved state and sends user intent
through the iOS FFI. See [`docs/modules/project.md`](../../../docs/modules/project.md)
for run, budget, stage, approval, and archive semantics.

## Shape

The navigation stack is:

```text
Projects tab -> board -> card
                    \-> activity
```

- The tab root lists projects and is the only project switcher.
- A board shows one stage at a time. The stage strip owns horizontal paging so
  row swipe actions and the edge-back gesture remain independent.
- A card uses a native header and composer around `issue.html`, a full-page
  WKWebView. Embedding that page in a native scroll view would create two
  scroll owners and require content-height round trips.
- A run transcript is a read-only `TranscriptTarget` presented from the card.

Native screens own navigation, forms, pickers, confirmation, attachments, and
write state. The card web page owns markdown, timeline layout, attachment
cards, sub-issue links, and scroll/fold state.

Card descriptions and comments use the transcript bundle's shared Markdown renderer.
Fenced code therefore has the same syntax highlighting and upper-right native-clipboard
copy control as chat and run transcripts; the card does not carry a second renderer.

## Data ownership

`ProjectsStore` owns the project list and open-board snapshots. It persists:

- `projects.json` for the project list, activity, and attention;
- `board-<id>.json` for a board's cards, active runs, and team;
- local project recency used only to order the phone's project list.

Remote snapshots replace mirrors wholesale. They are not merged field by
field. Logout or rebinding removes these account-scoped mirrors.

Each navigation visit owns an `IssueStore`. A card also has a replace-only
mirror so its first frame can paint before the five detail reads finish. Cached
data may render content, but it must not arm live-only actions:

- approvals require the current gateway approval queue;
- Stop requires a currently unsettled run;
- the unread divider comes only from a live timeline response.

`IssueHostPool` keeps two `IssueHost` renderers warm. Adjacent pages need two
simultaneously visible WKWebViews during a push or pop; deeper visits reuse the
slot hidden by the top two pages. Every visit has its own UUID, store, draft,
scroll position, and fold state, and every delayed bridge callback carries the
target UUID so work from an old card cannot land on the new occupant of a slot.

Run transcripts are not mirrored. `ProjectRunReadStore` fetches the newest page
as a sync baseline, then uses backward history pages for scrolling. While a run
is live it refreshes on project invalidations and falls back to polling when
frames are unavailable.

## Invalidations

`Frame::ProjectChanged` is a connection-global invalidation, not a patch. The
Rust pump consumes it and publishes it through `ProjectInvalidations`; visible
stores decide whether the project id, issue number, and scope affect them.
`Gap { session_id: nil }` invalidates project state as well.

Any affected store refetches authoritative state. A card move can renumber an
entire column, so applying a local per-card delta would not converge reliably.

## Writes

The app does not reproduce board scheduling rules. It submits the requested
move, assignment, comment, retry, approval, or settings change and renders the
server response or error.

- Issue edits use PATCH semantics; omitted fields are unchanged and
  `StringPatch` represents keep, clear, and set across UniFFI.
- Project settings use PUT full-replacement semantics, so every saved field is
  sent, including `agents_may_merge` and both budget ceilings.
- Board mutations have no offline replay queue. Replaying a move or assignment
  after the board changed would apply stale intent.
- Comments are the exception: `IssueCommentOutbox` persists an optimistic row
  with a client UUID. Retries reuse that UUID, and the gateway returns the
  original timeline row instead of repeating consequences.
- Optimistic board writes carry a revision. A failed older write may not roll
  its captured snapshot over a newer refresh or mutation.

Archived projects remain readable. Mutable board actions and approvals stay
disabled until restoration; marking read and stopping already-running work are
still allowed by the server contract.

## Presentation invariants

- Card reading order is pinned, then unread, then most recently updated.
  Position and issue number only break ties; this is display-only and never
  writes column order.
- Cancelled cards remain reachable but do not count as live work.
- Red is reserved for action signals such as an approval, unread activity, or
  a failed run. Priority and ordinary status use state colours instead.
- Budget holds are standing conditions, shown beside the setting that can lift
  them rather than as unread activity.
- Moving out of In Progress does not stop its run. Stop is a separate action.
- Moving or creating a staffed card in In Progress may start work; the UI must
  not claim that work started until the server confirms it.
- The app-icon badge remains chat-only. Project attention is foreground data
  because board changes are not delivered through APNs.

## Related docs and checks

- [`connection.md`](connection.md) describes the `ProjectSink` pump lane.
- [`navigation.md`](navigation.md) describes the tab and pushed-screen shell.
- [`testing.md`](testing.md) lists the project demo flags, Swift/UI suites, and
  the hand-written DTO drift sentinel.
- [`design-system.md`](design-system.md) owns visual tokens and state colours.

The relevant local checks are the iOS FFI tests, the `app/ios/web` lint/test/
build, Swift unit tests, and the Projects UI tests. The root workspace and iOS
workspace are separate; root Cargo commands do not cover this app.
