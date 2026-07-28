# Navigation shell and Liquid Glass

*The app's navigation shell (`HomeTabView`, the outer `NavigationStack`, the interactive pop gesture) and the Liquid Glass surfaces layered on it — `app/ios/App/Screens/RootView.swift`, `app/ios/App/Screens/HomeScreen.swift` (which declares `HomeTabView`), `app/ios/App/Support/PopGesture.swift`, `app/ios/App/Support/Theme.swift`.*

## Navigation

### The home shell is a native TabView

The home shell (`AppStore.Route.home`) is `HomeTabView`, a NATIVE
`TabView(selection: $homeTab)` (Liquid Glass bar on iOS 26+, the classic system
bar on 18–25 — system chrome, degrades on its own) with four sections
(Deck · Projects · Chats · Settings, `AppStore.HomeTab`).

- `deck` (`DeckScreen` — the board of agent-authored live cards, see
  [deck.md](deck.md) and `docs/modules/deck.md`),
- `chats` (`ChatListScreen`) and
- `settings` (`SettingsScreen` — language, version, log out)

have real screens; `projects` is `PlaceholderScreen`.

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

The plus is a stock SwiftUI `Menu` with two flat entries (Photos → the
`PhotosPicker`, Files → a `.fileImporter`) — stock, not a hand-rolled panel like
`ModelMenuPanel`, which exists only because it needed three levels and trailing
checkmarks. It stays INLINE in the one pill at its 46×48 frame: no satellite
icon circles, and the `Menu` carries that frame explicitly because an
unconstrained menu label grows greedily and eats the field beside it. What the
menu stages is [attachments.md](attachments.md) § Outbound staging.

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
