/// Collapse the gap between the chat composer and the on-screen keyboard.
///
/// iOS keeps reporting the home-indicator safe-area inset while the keyboard is
/// up, even though the keyboard already covers that strip — so the bottom-pinned
/// composer floats a ~34px band above the keyboard. CSS can't tell on its own when
/// the keyboard is up, so we watch the viewport and toggle `html.kb-open`; the chat
/// screen then drops that redundant inset (see styles.css). We deliberately do NOT
/// touch the chat height — iOS already sizes the webview/viewport to the area the
/// keyboard leaves, so the only stray space is that inset.
///
/// Detection is signal-agnostic: iOS may either overlay the keyboard (the visual
/// viewport shrinks) or resize the webview frame (`window.innerHeight` shrinks), so
/// we track the smaller of the two against the largest ("no keyboard") value seen
/// in the current orientation.
///
/// No-op without the VisualViewport API (shipped in iOS WKWebView since iOS 13).

/// A visible-height drop smaller than this is viewport noise (sub-pixel rounding,
/// a hardware-keyboard bar), not a software keyboard; collapsing the inset then
/// would let the composer hug the home indicator.
const KEYBOARD_MIN_DROP_PX = 60;

export function installKeyboardInsetTracking() {
  const vv = window.visualViewport;
  if (!vv) return;
  const root = document.documentElement;
  // Largest visible height seen this orientation = the no-keyboard baseline; the
  // keyboard only ever shrinks the visible area below it.
  let baseline = 0;

  const apply = () => {
    const visible = Math.min(window.innerHeight, vv.height);
    if (visible > baseline) baseline = visible;
    root.classList.toggle("kb-open", baseline - visible > KEYBOARD_MIN_DROP_PX);
  };

  apply();
  vv.addEventListener("resize", apply);
  vv.addEventListener("scroll", apply);
  // Rotation changes the no-keyboard height, so re-seat the baseline.
  window.addEventListener("orientationchange", () => {
    baseline = 0;
    apply();
  });
}
