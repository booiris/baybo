# Navigation shell and Liquid Glass

*The app's navigation shell (`HomeTabView`, the outer `NavigationStack`, the interactive pop gesture) and the Liquid Glass surfaces layered on it — `app/ios/App/Screens/RootView.swift`, `app/ios/App/Screens/HomeScreen.swift` (which declares `HomeTabView`), `app/ios/App/Support/PopGesture.swift`, `app/ios/App/Support/Theme.swift`.*

## Navigation

### The home shell is a native TabView

The home shell (`AppStore.Route.home`) is `HomeTabView`, a NATIVE
`TabView(selection: $homeTab)` (Liquid Glass bar on iOS 26+, the classic system
bar on 18–25 — system chrome, degrades on its own) with five sections
(Deck · Projects · Chats · Settings · Search, `AppStore.HomeTab`).

- `deck` (`DeckScreen` — the board of agent-authored live cards, see
  [deck.md](deck.md) and `docs/modules/deck.md`),
- `projects` (`ProjectsScreen` — one card per board, pushing a board and then a
  card, see [projects.md](projects.md)),
- `chats` (`ChatListScreen`),
- `settings` (`SettingsScreen` — language, version, log out) and
- `search` (`SearchScreen` — full-text over every conversation, see
  [chat-list.md](chat-list.md#searching-conversations)).

**Tab badges.** Chats and Projects each carry a count; the other three never do.
Chats reuses the very number the app icon carries (`BadgeCenter.total`), so the
two cannot disagree by construction. Projects sums what every live board is
waiting on — approvals + failed runs + unread, exactly the set
`/projects/attention` reports — and deliberately excludes runs the daily ceiling
is holding: a hold is a standing condition, not an event, and a badge that
cannot be cleared by acting is worse than no badge.

Two traps, both found on a simulator and neither visible in code:

- **SwiftUI exposes `.badge` to accessibility NOWHERE.** The tab item's label
  stays the bare section name and the badge has no child element. `ProjectsUITests`
  therefore asserts the drawn disc in PIXELS
  (`ScreenshotPixels.redCoverage(in:)`), with Deck as the no-badge control — a
  test reading `label` would pass a build that drew no badge at all.
- **`AppStore.projectsStore` is a nested `ObservableObject`,** so its changes do
  not republish `AppStore`. `HomeTabView` subscribes to `$attention` directly;
  reading the count through `store` froze it at whatever the first paint saw.

**Why search does NOT use `.searchable` / the iOS 26 tab-bar morph.** On 26,
selecting a search-role tab can turn the tab bar itself into a search field. It
needs a navigation bar to host that field, and this shell has none: the
`.toolbar(.hidden, for: .navigationBar)` applied to `HomeTabView` propagates into
nested `NavigationStack`s. Measured on 26.5 — `.searchable` on the tab content,
on an inner stack with the bar hidden, with it forced `.visible`, and on the
`TabView` itself — the field rendered in the accessibility tree in **none** of
them, and `.navigationTitle` on that inner stack did not render either, which is
what proves the bar never exists. Getting the morph means dropping the shell-wide
hide in favour of per-destination hiding: a `RootView` refactor touching every
pushed screen's chrome. It is NOT a deployment-target question — `#available(iOS
26.0, *)` was true throughout.

So the bottom morph is hand-rolled instead, and ONLY the bottom: selecting search
hides the native tab bar and `SearchScreen` docks its own field where the bar
was, growing it from the search circle's footprint. The bar is untouched
everywhere else, so the Liquid Glass selection morph is still the system's. This
is NOT the "bar pops back in after the transition" glitch below — that one is a
pushed screen on an inner stack revealing the bar on the POP; here nothing is
pushed and the bar hides and returns on tab SELECTION, in place.

**Search is the one tab with a `role`.** `Tab(..., role: .search)` is what makes
iOS 26 lift it OUT of the glass pill and float it as a detached circle at the
trailing edge; without the role it would just be a fifth item inside the pill.
`TabRole` is iOS 18.0+ — exactly the deployment target — and on 18–25 it renders
as an ordinary tab, so nothing here branches on version. It is also the only tab
whose screen skips the shared wordmark header: its search field IS its header.

### The NavigationStack wraps the WHOLE TabView

An OUTER `NavigationStack(path: $chatPath)` in `RootView` WRAPS the whole
TabView; opening a session pushes `ChatScreen` over the ENTIRE shell (tab bar
included), so the bar reveals together with the pop transition.

**Do NOT move the stack inside the Chats tab and hide the bar with
`.toolbar(.hidden, for: .tabBar)`** — that reappears the bar abruptly AFTER the
pop, the "bar missing then pops in" glitch.

### Session creation and tab routing

No session is minted at launch or login; the compose button — the Chats header's
top-right glass circle — is the only session creator, and compose / push-tap
routing force `homeTab = .chats` (in `activateSession`) so a pushed conversation
lands in the Chats stack.

**One exception, `keepTab: true`** — a conversation opened from a SEARCH hit
leaves the selection alone, so backing out of it returns to the results that
found it rather than to the chat list. That is safe precisely because of the
section above: the push covers the whole TabView, so the tab underneath is
untouched and simply reappears on the pop.

### Tint, and why compose is not in the tab bar

`.tint(Theme.ink)` colours the selected tab item ink (the HIG blesses a
monochromatic tab bar); the selection capsule is the system Liquid Glass
material — no public API recolours it and none is wanted (neutral glass, no
forced blue).

Compose is NOT in the tab bar: the native bar is for navigation not actions
(HIG) and exposes no slot for a custom button — an earlier custom glass pill bar
(with a separate compose circle) was dropped for exactly this, to get the native
selection morph.

### PopGestureEnabler / PopVelocityClamp

The `ChatScreen` still hides the system nav bar (custom chrome), which disables
UIKit's interactive pop — `PopGestureEnabler` (attached to ChatScreen)
re-enables the edge-swipe back with a root + in-flight-transition guard, hands
the delegate back on disappear, and clamps `velocityInView:` (dynamic subclass,
`PopVelocityClamp`) so iOS 26's fluid pop can't inherit a fast flick's velocity
and overshoot the revealed list (the "list slides right then rubber-bands"
glitch; stock Settings does the same, it just hides it better).

The clamp install is `#available(iOS 26, *)`-gated: pre-26 pops don't overshoot,
and UIKit's finish/completion math reads the same velocity — capping it there
would only make fast flicks feel laggy.

### Lending the edge out — `EdgeSwipeOverride`

A screen that lets something COVER the conversation has to lend the left edge to
it: swiping out of a full-screen overlay must leave the overlay, not the chat
under it. `PopGestureEnabler(edgeOverride:)` takes that reading — `active`, plus
`begin` / `move(points)` / `end(dismiss)` — and while it is active the host

- refuses the interactive pop AND, on iOS 26, disables
  `interactiveContentPopGestureRecognizer` with it. Refusing only the edge one
  would be worse than doing nothing: the content recognizer is ordered BEHIND it
  by a failure requirement, so an edge recognizer that steps aside simply hands
  the pop over;
- arms its own `UIScreenEdgePanGestureRecognizer` on the NAVIGATION
  controller's view (a recognizer sees every touch in its view's subtree, which
  is what lets it read a swipe landing on web content — the zero-sized host view
  takes no touches at all), and
- judges the release itself: past `dismissFraction` of the width, or a flick
  over `dismissVelocity`. The renderer is only ever told the verdict.

Its one caller today is `ChatScreen`, for the full-screen HTML preview — which
lives inside the transcript webview, so native owns the gesture and the page
owns the travel (see [`transcript.md`](transcript.md)).

## Liquid Glass (iOS 26+; deployment target is 18.0)

### Never call `.glassEffect` raw

Every CUSTOM glass surface goes through `Theme.swift`'s
`glassSurface(tint:interactive:in:)` — the real `.glassEffect` on 26+, an
`.ultraThinMaterial` fill in the same shape below (strokeless on purpose: the
composer pill is borderless and the jump button is bare).

**Never call `.glassEffect` raw** — it is 26-only and an unguarded call breaks
the 18.0 target.

The one other 26-only API, `interactiveContentPopGestureRecognizer`, is
`#available`-guarded in `PopGesture.swift` (pre-26 the edge recognizer alone
carries the feature).

Building still REQUIRES Xcode 26 / the iOS 26 SDK at any deployment target —
`#available` gates runtime, not compilation, and the guarded branches reference
26-SDK-only symbols (`Glass`, the content-pop recognizer).

### The tab bar is the system's glass, not ours

The bottom tab bar is the NATIVE `TabView` Liquid Glass bar — its
selection-capsule morph (the glass that slides + stretches between tabs) is the
SYSTEM's, and getting that authentic morph is exactly why the custom bar was
dropped. Kept monochrome via `.tint(Theme.ink)` (ink selected item, neutral
system-glass capsule, no accent hue); tab icons are thin line SF Symbols
(`waveform.path.ecg`/`square.stack.3d.up`/`message`/`gearshape`).

The remaining CUSTOM glass surfaces are the chat composer dock, the
jump-to-latest button, and the Chats header's compose circle
(`square.and.pencil`) — a recorded deviation from the flat-monochrome system in
[design-system.md](design-system.md), which still governs everything else.

### History (see git log, don't re-tread)

A custom glass pill bar was built first — `matchedGeometry` chip → then a
`GlassEffectContainer`+`glassEffectID` morph (which cross-faded on far hops and
threw a red chromatic fringe) → then a single sliding-`.position` lozenge with a
drag gel-stretch; none matched the native selection stretch, so we went native
`TabView` (the native bar can't host the separate compose action circle — HIG:
tab bar is navigation, not actions — so compose moved to the Chats header
top-right).

### The composer pill

The composer is ONE ChatGPT-style glass pill (inline plus on the left, in-field
ink send circle on the right; at rest it holds a moderate width, and focus
stretches it toward the screen edges — a small gutter stays — on the keyboard's
beat).

The plus opens `AttachMenuPanel` — flat rows (Photos → the `PhotosPicker`,
Files → a `.fileImporter`, plus Paste when the clipboard holds an image, so the
panel is two rows tall or three) — a HAND-ROLLED panel, `ModelMenuPanel`'s sibling,
and NOT the stock `Menu` it shipped as first. A SwiftUI `Menu` is a `UIMenu`: it
dims the whole screen, lifts its anchor view into a system layer and puts the
bubble where it decides. This one has to leave the pill exactly where it is,
un-dimmed and still tappable, and bloom UPWARD out of the `+`. It stays INLINE
in the one pill at its 46×48 frame: no satellite icon circles. What the panel
stages is [attachments.md](attachments.md) § Outbound staging.

`ChatScreen` owns the state (`AttachMenu`) and presents the panel in **two
layers**, which is the whole design:

- the **scrim** (`AttachMenuScrim`) lands in the screen's ZStack, which the
  dock's `.safeAreaInset` composites ABOVE — so it dims the transcript and the
  header and NOT the dock. That layering is what leaves the pill un-dimmed and
  live, and it is what `plus.isHittable` in `ComposerAttachUITests` pins;
- the **panel** is an `.overlay` on the DOCK's content, because that is the only
  layer that stacks over the dock's own rows. Presented in the ZStack it drew
  UNDER everything the dock grows upward — the notice line, the approval card,
  the staged strip — and under the jump-to-latest disc, 44pt of which took the
  Files row's taps. An overlay adds nothing to the inset, so the transcript's
  bottom inset is still what `ComposerView` alone measures.

**The panel's floor is the composer's top edge, and the jump disc passes under
it** (2026-08-27). The disc used to be a ROW in the dock's stack, which made it
part of the panel's floor: raising the disc raised the whole panel by 56pt, so
pressing `+` with the disc up opened the menu a disc's height clear of the `+`
that opened it. `JumpToLatestDisc` is an `.overlay` on the composer now —
`lift = 44 + 12` above its top edge, the same place it drew before — so the
dock's content is the composer again and the panel opens in one place whether
or not the disc is up. Covering the disc is only safe because the panel is
ABOVE it in the same layer; the taps in the overlap go to the panel, which is
the half `ComposerAttachUITests` and `IssueDockUITests` each pin with an
`intersects` + `isHittable` pair.

Its column is MEASURED, never hardcoded the way the header's `anchorLeading` can
be: the `+` reports its own frame (`AttachMenu.report`) and `AttachMenuPanel.box`
puts the panel's leading edge on it, because the pill's horizontal padding
animates between 40 at rest and 14 focused. That frame republishes only while
the panel is UP — it changes on every tick of the focus and keyboard animations,
the same reason `setComposerTop` publishes nothing at all.

Its FLOOR is the dock's top edge, `anchorGap` below the panel's bottom — not the
`+`'s top edge, which is the shape this shipped as and the bug: the `+` sinks
INTO the dock as those rows stack above it, so against a 4-tile staged strip 68
of the panel's 92pt (two rows, as it then was) drew behind the strip with the
Files row hidden and untappable, and behind an approval card nothing showed at
all. `box` works in the DOCK's own coordinate space (`AttachMenuPanel.dockSpace`,
the space the `+` reports in), so its whole box is negative — it floats above
`y = 0`. Its height is `rowHeight × rows` COMPUTED, never measured: an `.offset`
positions the panel, and an offset needs the height before the view lays out.
That is why the shown rows are passed in (`sources`) rather than read off
`AttachSource.allCases` — a conditional Paste row would otherwise be reserved
space it does not use, floating the panel a whole row too high. Measuring in one
container and drawing in another is what put the live touch region ~14pt below
the paint: the top of the Photos row dismissed the panel and the empty gap under
it fired Files.

Constraints: white tint only, no `.interactive()` shimmer on the field, the pill
is BORDERLESS — a soft ink shadow carries its boundary over the blank at-rest
strip (no hairline); the jump button is bare glass (no stroke).

### The dock's paper veil

The dock's paper veil (bottom mirror of the header veil) is load-bearing, not
decoration: it hit-tests the dock rect (gutter taps must not scroll the webview)
and masks the web-vs-native inset animation phase mismatch. Its fade spans the
dock itself — alpha 0 at the dock's top edge, peak only under the PILL'S BOTTOM
edge — so scrolled content ghosts past the pill's flanks.

### The jump button

The jump button is native: web posts `jumpVisible` on its `showJump` state,
native taps call `jumpToLatest` back; the composer-top geometry is measured on
the ComposerView alone so the button never inflates the web inset.

One view, `JumpToLatestDisc`, for both pages — the chat's thread and a card's
Activity draw the same 44pt disc 12pt above their dock, and it is an OVERLAY on
the composer rather than a row above it, so it costs the dock no height and the
attach panel opens over it rather than above it (see the panel's layering
above). The card raises it off `onActivityAtBottom` rather than `jumpVisible`,
which is the only difference left between the two.
