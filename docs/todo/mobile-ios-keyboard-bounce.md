# TODO: iOS keyboard "bounce" in the mobile chat composer

**Status:** known issue, **deferred** (not fixed). This doc captures the diagnosis and
the proven fix so the future change is mostly implementation.

Scope: `app/mobile` (the Tauri v2 / WKWebView iOS app). Desktop and Android are
unaffected.

## Symptom

When the user focuses the chat composer (`app/mobile/src/App.tsx`, the `.composer
textarea`), the page **bounces once** as the keyboard rises: the composer overshoots
*upward* past its resting spot, then settles just above the keyboard. On dismissal it
can bounce back. Confirmed on-device (frame-by-frame from a screen recording: the
composer leaves the top of a y-tracking crop window during the rise, then drops back).

## What already works — DO NOT re-break these

These shipped and are **kept**; they are separate from the bounce:

- **Accessory bar removed** — `keyboard::hide_input_accessory_bar` swizzles the private
  `WKContentView.inputAccessoryView` getter to `nil` (`app/mobile/src-tauri/src/keyboard.rs`).
  The strip still visible above the keyboard is iOS's own QuickType bar, not ours.
- **Composer is a `<textarea>`** — Enter inserts a newline; the Send pill sends; it
  auto-grows to a max height (`App.tsx`, `styles.css .composer textarea`).
- **Resting gap is tight** — `app/mobile/src/viewport.ts` tracks the visual viewport and
  toggles `html.kb-open`; `styles.css html.kb-open .chat { padding-bottom }` drops the
  redundant home-indicator safe-area inset while the keyboard is up, so the composer sits
  ~0.75rem above the keyboard at rest (user-confirmed "贴得很近"). This is the
  steady-state placement and is **not** the bounce.

## Root cause (it is native WKWebView, not our CSS)

Two independent **native** mechanisms fire on the same focus event, on different
animation timelines, and over-correct:

1. **WebKit's scroll-to-focused-element.** In the WebKit UIProcess, the focused editable
   node is scrolled/zoomed to land inside the unobscured content rect
   (`-[WKContentView _zoomToRevealFocusedElement]`). WebKit tries to time this against the
   keyboard slide (`isKeyboardAnimatingIn()` / `_waitingForKeyboardToStartAnimatingInAfterElementFocus`);
   the existence of that deferral machinery is the tell that the coordination is fragile.
2. **UIKit's automatic keyboard inset.** On `UIKeyboardWillShow`, UIKit injects a bottom
   content-inset (= keyboard height) into the webview's `UIScrollView` and adjusts its
   `contentOffset`. The layout viewport stays full-height while the visual viewport shrinks
   and is shifted up to keep the focused input visible.

A bottom-pinned composer gets pushed *up* past its resting spot (the overshoot), then a
post-layout reconcile snaps it to the final rect (the settle).

**Why web-only fixes can't win:** both run natively, *before* any page JS. `visualViewport`
events are not cancelable and arrive a frame late; there is no web API to opt out of
`_zoomToRevealFocusedElement`; `interactive-widget=resizes-content` is **unsupported in
WebKit**; `overflow:hidden` only governs the document scroller, not WebKit's reveal +
the layout-viewport shift. Every framework that actually fixes this (Capacitor/Ionic,
Apache+Ionic Cordova, react-native-webview) does it **natively**.

## wry / Tauri specifics (verified against wry 0.55.1 source)

- wry creates the WKWebView in `src/wkwebview/mod.rs`; the **only** scroll-view config it
  does is `scrollView.setBounces(false)` (~lines 525–530). It does **not** set
  `contentInsetAdjustmentBehavior`, and wry/tao do **zero** keyboard handling
  (`tao/src/platform_impl/ios/mod.rs` literally `// todo: implement iOS keyboard event`).
- wry exposes **no** builder/runtime knob for iOS scroll/keyboard/inset. The fix must be
  applied natively to the live `scrollView`.
- We reach the WKWebView from Rust via Tauri `with_webview` (runs the closure on the UI
  thread): `win.with_webview(|pw: tauri::webview::PlatformWebview| { let wk = pw.inner() as *mut AnyObject; ... })`.
  `PlatformWebview::inner()` returns `*mut c_void` (the `WryWebView`, a `WKWebView` subclass).

## Diagnostic finding (important for the fix)

A one-line attempt (`scrollView.contentInsetAdjustmentBehavior = .never`, value 2) was wired
via `with_webview` and **confirmed to run** — the on-device log printed:

```
baybo: tame_keyboard_scroll: contentInsetAdjustmentBehavior 3 -> 2
```

Two takeaways:

- The **starting value was `.always` (3)**, not the `.automatic` (0) the generic research
  assumed. Something (WKWebView/iOS default for this version) sets it aggressively.
- Setting `.never` stuck at that instant **but the bounce persisted**. So either the value
  is reset back to `.always` when the keyboard actually shows, or `.never` alone is
  insufficient (it does not touch the layout-viewport shift). **Conclusion: a one-time
  setup-time set is not enough — the fix must re-apply on every keyboard event.**

## What was tried and reverted (don't repeat as-is)

All removed from the tree (this is why they're listed — so we don't loop):

1. **Web: `.chat { height: var(--app-vh) }`** driven by `visualViewport.height` — caused a
   regression (composer pushed far) because `visualViewport.height` did not match the
   visible height in this webview. Reverted.
2. **Web: `html.chat-open { overflow:hidden }`** document scroll-lock — does not touch the
   native scrollView/reveal; bounce unchanged. Reverted.
3. **Web: `transition: padding-bottom`** on `.chat` — only smoothed the kept inset toggle,
   not the bounce. Reverted.
4. **Native: one-time `contentInsetAdjustmentBehavior = .never`** via `with_webview` in
   `setup` + re-assert on `Resumed` — confirmed to run (`3 -> 2`) but bounce persisted (see
   above). Reverted.

## Recommended fix (proven, native, per-keyboard-event)

Port the Capacitor/Ionic "native resize" recipe into `app/mobile/src-tauri/src/keyboard.rs`
(we already swizzle there) and wire it from `lib.rs` `setup` via `with_webview`. On the
live `webView.scrollView`:

1. Set `contentInsetAdjustmentBehavior = .never` (and keep wry's `bounces = false`).
2. **Observe `UIKeyboardWillShowNotification` / `UIKeyboardWillHideNotification`** (objc2:
   `NSNotificationCenter`, a `define_class!` observer kept alive in a `thread_local`
   `Retained<…>`). On show: read the keyboard height from the notification `userInfo`
   (`UIKeyboardFrameEndUserInfoKey`), **shrink `webView.frame.height` by it** so the webview
   physically sits above the keyboard (no overlap ⇒ no scroll-to-focus, no overshoot), and
   zero `scrollView.contentInset` + pin `contentOffset`. On hide: reverse.
3. Re-applying on every keyboard event covers both failure modes from the diagnostic (value
   reset back to `.always`, and `.never`-insufficient). This is the highest-reliability
   approach and is what Capacitor ships by default.

Staged fallbacks if the full resize is too heavy:
- **A — pin `contentOffset`.** Class-swizzle the live scrollView's concrete class
  (`object_getClass(scroll)`, the private `WKScrollView`) `setContentOffset:` /
  `setContentOffset:animated:` to no-op (same `class_replaceMethod` pattern as the existing
  accessory-bar swizzle). Safe **only** while the document never legitimately scrolls — the
  chat does not (the message list is a separate WebKit overflow sub-scroller), but other
  screens (landing/connected) can, so scope it on/off rather than leaving it global.
- **B — KVO** the scrollView's `contentOffset` and reset to zero during the keyboard window
  (avoid setting `scrollView.delegate`, which can interfere with WebKit gestures).

Orthogonal nicety: ensure `input/textarea` `font-size >= 16px` to avoid the separate
focus-zoom artifact (today the composer is `1rem` = 16px, so this already holds).

## App Store note

The minimal `contentInsetAdjustmentBehavior` setter and the keyboard-notification approach
use **public** UIKit/UIScrollView APIs only — no private class, so no new private-API
surface beyond the existing `WKContentView` accessory-bar swizzle. The optional
`setContentOffset:` swizzle touches the private `WKScrollView`'s concrete class but only via
public selectors + the runtime — the same posture already shipping for the accessory bar.

## References

- Capacitor: "Webview jumps up and down when keyboard is about to show" — github.com/ionic-team/capacitor/issues/1366
- Ionic: "Opening keyboard pushes window up (WKWebView)" — github.com/ionic-team/ionic/issues/4230
- WebKit #192564 — keyboard dismiss leaves `viewport-fit=cover` content offscreen (fixed via `setNeedsLayout`)
- WebKit change deferring `-_zoomToRevealFocusedElement` until the keyboard finishes animating (mail-archive webkit-changes msg185569)
- Tauri keyboard-bounce objc2 port via `with_webview` — github.com/tauri-apps/tauri#9368 (and #9907)
- wry: set `scrollView.bounces = false` — github.com/tauri-apps/wry#557
- Capacitor Keyboard plugin (resize modes native/body/ionic/none, `setScroll`) — capacitorjs.com/docs/apis/keyboard
- Cordova `CDVWKWebViewEngine.m` (`contentInsetAdjustmentBehavior=.never`, keyboard-hide offset clamp) — github.com/ionic-team/cordova-plugin-ionic-webview
