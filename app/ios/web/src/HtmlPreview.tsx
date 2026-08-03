import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { flushSync } from "react-dom";
import { useTranslation } from "react-i18next";
import { postHtmlPreviewMaximized } from "./bridge";
import {
  HTML_PREVIEW_COLLAPSE_EVENT,
  HTML_PREVIEW_DRAG_BEGIN_EVENT,
  HTML_PREVIEW_DRAG_END_EVENT,
  HTML_PREVIEW_DRAG_MOVE_EVENT,
  HTML_PREVIEW_MAXIMIZED_CLASS,
  htmlPreviewUrl,
} from "./htmlPreviewProtocol";

type Rect = { left: number; top: number; width: number; height: number };

/// Upper BOUND on the morph, not a mirror of it. The animations themselves are
/// what normally retire it, and the duration itself lives in
/// `--html-preview-morph` (styles.css) so nothing here can drift from it. This
/// exists only because a dropped `transitionend` must never strand a
/// screen-covering element.
const MORPH_FALLBACK_MS = 400;

/// How expanded the box is: 0 = the inline card, 1 = full screen. styles.css
/// derives every non-rect difference between the two states from it (the safe
/// areas it clears, its border, its corners), so writing it here is what makes
/// the morph's end state the FINAL state. The class sets it too, but only at
/// the far end — so a dismissal writes 0 one morph early and a drag rides it
/// down with the finger.
const EXPANSION_VAR = "--html-preview-expansion";
const COLLAPSED = "0";
/// How much of the morph's duration is still owed, as a fraction (styles.css
/// multiplies its base duration by this).
const MORPH_SCALE_VAR = "--html-preview-morph-scale";
/// Floor on that scaling. Two reasons, from opposite directions: a transition
/// scaled to nothing never runs (so nothing ever reports it finished, and the
/// box waits out the whole fallback), and a tail squeezed under ~5 frames stops
/// reading as motion at all — measured at 0.15, the last chunk of a long drag
/// landed in a single rendered frame, which is a jump however you time it.
const MIN_MORPH_SCALE = 0.35;
/// Under a pixel of travel left is not a morph. Animating it buys nothing and
/// costs a `transitionend` that may never come, since a property that does not
/// change does not transition.
const STILL_PX = 1;

function writeRect(element: HTMLElement, rect: Rect): void {
  element.style.left = `${rect.left}px`;
  element.style.top = `${rect.top}px`;
  element.style.width = `${rect.width}px`;
  element.style.height = `${rect.height}px`;
}

/// The far end of the morph, in VIEWPORT UNITS rather than the pixel count it
/// resolves to right now — a rotation or a dynamic-viewport change then stays
/// correct with nobody re-measuring.
function writeFullScreen(element: HTMLElement): void {
  element.style.left = "0px";
  element.style.top = "0px";
  element.style.width = "100vw";
  element.style.height = "100dvh";
}

function measure(element: Element): Rect {
  const box = element.getBoundingClientRect();
  return { left: box.left, top: box.top, width: box.width, height: box.height };
}

/// The largest single-axis gap between two rects — "how far is there still to
/// go", in the only terms that matter for whether a morph is worth running.
function rectDistance(a: Rect, b: Rect): number {
  return Math.max(
    Math.abs(a.left - b.left),
    Math.abs(a.top - b.top),
    Math.abs(a.width - b.width),
    Math.abs(a.height - b.height),
  );
}

function lerpRect(from: Rect, to: Rect, t: number): Rect {
  return {
    left: from.left + (to.left - from.left) * t,
    top: from.top + (to.top - from.top) * t,
    width: from.width + (to.width - from.width) * t,
    height: from.height + (to.height - from.height) * t,
  };
}

/// The glyph set is stroked line art on a 20-unit box at 1.2 weight, the same
/// hand as the file/audio/video cards in Transcript.tsx.
const GLYPH_WINDOW = (
  <>
    <rect
      x="2.9"
      y="4.2"
      width="14.2"
      height="11.6"
      rx="2.2"
      stroke="currentColor"
      strokeWidth="1.2"
    />
    <path d="M2.9 8.1h14.2" stroke="currentColor" strokeWidth="1.2" />
  </>
);

/// A circle opened at the top right, with the arrow head sitting in the gap.
const GLYPH_RELOAD = (
  <>
    <path
      d="M15.4 6.3A6.2 6.2 0 1 0 16.2 10"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinecap="round"
    />
    <path
      d="M16.4 3.2v3.6h-3.6"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </>
);

/// Four corners pushed outward — take the whole screen.
const GLYPH_EXPAND = (
  <path
    d="M7.6 3.4H3.4v4.2M12.4 3.4h4.2v4.2M16.6 12.4v4.2h-4.2M3.4 12.4v4.2h4.2"
    stroke="currentColor"
    strokeWidth="1.2"
    strokeLinecap="round"
    strokeLinejoin="round"
  />
);

const GLYPH_CLOSE = (
  <path
    d="M5.9 5.9l8.2 8.2M14.1 5.9l-8.2 8.2"
    stroke="currentColor"
    strokeWidth="1.2"
    strokeLinecap="round"
  />
);

function ActionButton({
  label,
  glyph,
  onClick,
}: {
  label: string;
  glyph: ReactNode;
  onClick: () => void;
}) {
  return (
    <button type="button" className="html-preview-action" aria-label={label} onClick={onClick}>
      <svg viewBox="0 0 20 20" fill="none" aria-hidden="true">
        {glyph}
      </svg>
    </button>
  );
}

/// The maximize engine is Deck's, for the reason Deck's exists: an `<iframe>`
/// reloads when its node MOVES in the DOM, so the box never moves — it lifts to
/// `position: fixed` at the exact rect it already occupied and animates by
/// RECT. The `<pre>` around it holds that slot open (styles.css) so the collapse
/// has somewhere true to land, and the page inside is laid out at every
/// intermediate size rather than drawn scaled.
///
/// A fade was tried first, and cannot work here: the card can only be in one
/// place, so the moment the full-screen box drops below full opacity the reader
/// is looking at the EMPTY reserved slot behind it. Measured off a 60fps
/// capture: ~80ms of blank screen at the end of every dismissal, and a blank
/// first frame on every expand.
export function HtmlPreview({ blobId }: { blobId: string }) {
  const { t } = useTranslation();
  const [reload, setReload] = useState(0);
  const [loadedSrc, setLoadedSrc] = useState<string | null>(null);
  const [maximized, setMaximizedState] = useState(false);
  const maximizedRef = useRef(false);
  /// True from the instant a dismissal starts until its morph has run. The box
  /// must KEEP its full-screen geometry while it travels home, so `maximized`
  /// can only drop at the far end — everything that reads "is it up?" has to
  /// read this too, or a second dismissal restarts the first one's morph.
  const closingRef = useRef(false);
  const rootRef = useRef<HTMLSpanElement | null>(null);
  const settleTimer = useRef<number | null>(null);
  /// Bumped by every `cancelSettle`, so a settle that was superseded (a second
  /// morph, an unmount) can tell that its own wait no longer speaks for the box.
  const settleGeneration = useRef(0);
  /// The two ends a live edge drag interpolates between, captured once when the
  /// finger lands: measuring per frame would re-read a layout the drag itself is
  /// changing. `span` is how far the finger has to travel for the box to arrive
  /// all the way home.
  const dragRef = useRef<{ full: Rect; home: Rect; span: number; travelled: number } | null>(
    null,
  );
  const src = htmlPreviewUrl(blobId, reload);

  /// Drop a pending settle without running it — a re-open, an unmount, or a
  /// second morph starting on top of one still in flight.
  const cancelSettle = useCallback(() => {
    settleGeneration.current += 1;
    if (settleTimer.current === null) return;
    window.clearTimeout(settleTimer.current);
    settleTimer.current = null;
  }, []);

  /// Run `done` when the morph lands — meaning EVERY property it started has
  /// landed, not the first to report.
  ///
  /// This waited on `transitionend` and got it wrong: that event fires PER
  /// PROPERTY, and the morph moves seven of them. Whichever finished first
  /// retired the box while the rest were still a pixel or two short, and the
  /// class flip then snapped them — measured on a real page as the card's top
  /// edge jumping 7 device px with its border rules going from mid-transition
  /// widths to their final ones. `getAnimations()` is the whole set (and it
  /// flushes pending style, so the transitions this morph just started are
  /// already in it).
  const settle = useCallback(
    (root: HTMLElement, done: () => void) => {
      cancelSettle();
      const generation = settleGeneration.current;
      const run = () => {
        if (generation !== settleGeneration.current) return;
        cancelSettle();
        done();
      };
      // Guarded: an environment with no animation timeline at all (jsdom) has
      // nothing to wait for, and "nothing to wait for" is the right answer
      // there rather than a throw.
      const running = typeof root.getAnimations === "function" ? root.getAnimations() : [];
      if (running.length === 0) {
        run();
        return;
      }
      void Promise.allSettled(running.map((animation) => animation.finished)).then(run);
      // A dropped frame, a backgrounded page, an animation that never settles:
      // none of them may strand a screen-covering element.
      settleTimer.current = window.setTimeout(run, MORPH_FALLBACK_MS);
    },
    [cancelSettle],
  );

  /// Hand every inline style back. React never writes `style` on this element,
  /// so the imperative writes are ours alone to undo — which is also what lets a
  /// drag run at compositor speed without a re-render per frame.
  const resetStyles = useCallback(() => {
    const root = rootRef.current;
    if (root === null) return;
    root.style.cssText = "";
  }, []);

  /// Where the thread was parked when the box took the screen. Locking the
  /// document's scroller can clamp its offset, and the reader has to come back
  /// to exactly the row they left — not to wherever the clamp put them.
  const parkedScrollY = useRef(0);

  const applyMaximized = useCallback((next: boolean) => {
    maximizedRef.current = next;
    setMaximizedState(next);
    if (next) parkedScrollY.current = window.scrollY;
    document.documentElement.classList.toggle(HTML_PREVIEW_MAXIMIZED_CLASS, next);
    if (!next && window.scrollY !== parkedScrollY.current) {
      window.scrollTo({ top: parkedScrollY.current, behavior: "instant" });
    }
  }, []);

  const finishClose = useCallback(() => {
    cancelSettle();
    closingRef.current = false;
    dragRef.current = null;
    // Synchronous, and BEFORE the styles are dropped: the resting stylesheet
    // rule for `.is-maximized` is full screen, so a scheduled render would
    // leave a frame of un-pinned full-screen box between the two.
    flushSync(() => applyMaximized(false));
    resetStyles();
  }, [applyMaximized, cancelSettle, resetStyles]);

  /// `animated: false` is native's detach / session-switch collapse: the page is
  /// about to be repointed, so the box has to be gone this tick, not in a frame.
  const close = useCallback(
    (animated: boolean) => {
      if (!maximizedRef.current) return;
      if (closingRef.current) {
        if (!animated) finishClose();
        return;
      }
      // Native chrome comes back NOW, not when the morph lands: the box is
      // opaque and still covering the screen, so the header and composer are
      // already in place as it uncovers them.
      postHtmlPreviewMaximized(false);
      const root = rootRef.current;
      // The `<pre>` slot, measured at THIS instant rather than remembered from
      // the expand: rows keep arriving while a preview is up, so the card's home
      // may have moved down the thread since it left.
      const slot = root?.parentElement;
      if (!animated || root === null || slot === undefined || slot === null) {
        finishClose();
        return;
      }
      // Leaving `position: fixed` tears the box's compositing layer down and
      // repaints every glyph in it. That is unavoidable — but it must land WITH
      // the motion, not after it. A release that follows a long drag has almost
      // no distance left, and running the full duration anyway parked the box
      // for a quarter second and THEN flashed. So the morph is scaled to what is
      // actually left, and a box already home is retired on the spot.
      const current = measure(root);
      const home = measure(slot);
      if (rectDistance(current, home) < STILL_PX) {
        finishClose();
        return;
      }
      closingRef.current = true;
      const drag = dragRef.current;
      if (drag !== null) {
        const left = Math.max(MIN_MORPH_SCALE, 1 - drag.travelled);
        root.style.setProperty(MORPH_SCALE_VAR, String(left));
      }
      // Collapse everything the two states differ by NOW, so the box lands
      // already wearing its inline frame and insets. Left to the class flip,
      // the page inside grew a home indicator's worth of height at the bottom
      // the instant it landed.
      root.style.setProperty(EXPANSION_VAR, COLLAPSED);
      // Hand the transition back to the stylesheet (the settle froze it, and a
      // drag may have pinned it off), then flush layout — a geometry change
      // written in the same style recalc as the transition meant to animate it
      // is applied instantly instead.
      root.style.transition = "";
      void root.offsetHeight;
      writeRect(root, home);
      settle(root, finishClose);
    },
    [finishClose, settle],
  );

  const open = useCallback(() => {
    const root = rootRef.current;
    if (maximizedRef.current || closingRef.current || root === null) return;
    // One preview owns the screen at a time. This reaches every OTHER mounted
    // preview; our own listener no-ops on the guard above.
    window.dispatchEvent(new Event(HTML_PREVIEW_COLLAPSE_EVENT));
    cancelSettle();
    dragRef.current = null;
    const from = measure(root);
    resetStyles();
    // Pin the starting rect while the box is still IN FLOW — no transition is
    // declared there, so nothing animates. (`left`/`top` on a `position:
    // relative` box WOULD offset it, but nothing paints between here and the
    // class landing: `flushSync` puts it on in this same synchronous block.)
    writeRect(root, from);
    flushSync(() => applyMaximized(true));
    postHtmlPreviewMaximized(true);
    // Commit that start frame under the now-active transition, then travel.
    void root.offsetWidth;
    writeFullScreen(root);
    // Freeze the transition once expanded, so a later viewport change (a
    // rotation, the keyboard) resizes the box instantly rather than animating
    // it — and so a drag starts from a box that is already still.
    settle(root, () => {
      root.style.transition = "none";
    });
  }, [applyMaximized, cancelSettle, resetStyles, settle]);

  /// The finger drives the SAME morph the buttons animate: the box shrinks back
  /// toward its slot, it does not slide. Translating the whole page instead
  /// would be a second, unrelated motion that the release then has to hand over
  /// to a shrink — and it moves a full-screen page sideways, which reads as the
  /// conversation itself being dragged.
  const dragBegin = useCallback(() => {
    const root = rootRef.current;
    if (!maximizedRef.current || closingRef.current || root === null) return;
    cancelSettle();
    root.style.transition = "none";
    const slot = root.parentElement;
    dragRef.current =
      slot === null
        ? null
        : {
            full: measure(root),
            home: measure(slot),
            // A full screen-width drag arrives all the way home. Native commits
            // the dismissal well before that, so the release always has some
            // travel left to animate — the box never sits ON its slot with the
            // finger still down.
            span: Math.max(1, window.innerWidth),
            travelled: 0,
          };
  }, [cancelSettle]);

  const dragMove = useCallback((px: number) => {
    const root = rootRef.current;
    const drag = dragRef.current;
    if (!maximizedRef.current || closingRef.current || root === null || drag === null) return;
    const travelled = Math.min(1, Math.max(0, px / drag.span));
    drag.travelled = travelled;
    writeRect(root, lerpRect(drag.full, drag.home, travelled));
    root.style.setProperty(EXPANSION_VAR, String(1 - travelled));
  }, []);

  const dragEnd = useCallback(
    (dismiss: boolean) => {
      const root = rootRef.current;
      if (!maximizedRef.current || closingRef.current || root === null) return;
      if (dismiss) {
        // Carry on home. `close` re-measures the slot rather than reusing the
        // drag's copy: rows can land while a finger is down.
        close(true);
        return;
      }
      dragRef.current = null;
      root.style.transition = "";
      root.style.removeProperty(MORPH_SCALE_VAR);
      void root.offsetHeight;
      writeFullScreen(root);
      // Back to the stylesheet's full-screen values, animated by the same
      // transition that carries the box.
      root.style.removeProperty(EXPANSION_VAR);
      settle(root, () => {
        root.style.transition = "none";
      });
    },
    [close, settle],
  );

  useEffect(() => {
    const collapse = () => close(false);
    const onBegin = () => dragBegin();
    const onMove = (e: Event) => dragMove((e as CustomEvent<number>).detail);
    const onEnd = (e: Event) => dragEnd((e as CustomEvent<boolean>).detail);
    window.addEventListener(HTML_PREVIEW_COLLAPSE_EVENT, collapse);
    window.addEventListener(HTML_PREVIEW_DRAG_BEGIN_EVENT, onBegin);
    window.addEventListener(HTML_PREVIEW_DRAG_MOVE_EVENT, onMove);
    window.addEventListener(HTML_PREVIEW_DRAG_END_EVENT, onEnd);
    return () => {
      window.removeEventListener(HTML_PREVIEW_COLLAPSE_EVENT, collapse);
      window.removeEventListener(HTML_PREVIEW_DRAG_BEGIN_EVENT, onBegin);
      window.removeEventListener(HTML_PREVIEW_DRAG_MOVE_EVENT, onMove);
      window.removeEventListener(HTML_PREVIEW_DRAG_END_EVENT, onEnd);
    };
  }, [close, dragBegin, dragEnd, dragMove]);

  // Unmount is the one path that cannot animate — the element is going away, so
  // the document class and native's hidden chrome have to be handed back here or
  // they outlive the preview that hid them.
  useEffect(
    () => () => {
      cancelSettle();
      if (!maximizedRef.current) return;
      maximizedRef.current = false;
      closingRef.current = false;
      document.documentElement.classList.remove(HTML_PREVIEW_MAXIMIZED_CLASS);
      postHtmlPreviewMaximized(false);
    },
    [cancelSettle],
  );

  return (
    <span
      ref={rootRef}
      className={maximized ? "html-preview is-maximized" : "html-preview"}
      role="group"
      aria-label={t("chat.htmlPreview")}
    >
      <span className="html-preview-toolbar">
        <span className="html-preview-title">
          <svg className="html-preview-glyph" viewBox="0 0 20 20" fill="none" aria-hidden="true">
            {GLYPH_WINDOW}
          </svg>
          <span className="html-preview-label">{t("chat.htmlPreview")}</span>
        </span>
        <span className="html-preview-actions">
          <ActionButton
            label={t("chat.reloadHtmlPreview")}
            glyph={GLYPH_RELOAD}
            onClick={() => {
              setLoadedSrc(null);
              setReload((value) => value + 1);
            }}
          />
          <ActionButton
            label={maximized ? t("chat.closeHtmlPreview") : t("chat.expandHtmlPreview")}
            glyph={maximized ? GLYPH_CLOSE : GLYPH_EXPAND}
            onClick={() => {
              if (maximized) close(true);
              else open();
            }}
          />
        </span>
      </span>
      {loadedSrc !== src && (
        <span className="html-preview-loading" role="status">
          <span className="html-preview-spinner" aria-hidden="true" />
          {t("chat.loadingHtmlPreview")}
        </span>
      )}
      <iframe
        className={loadedSrc === src ? "html-preview-frame is-loaded" : "html-preview-frame"}
        src={src}
        title={t("chat.htmlPreviewFrameTitle")}
        sandbox="allow-scripts"
        referrerPolicy="no-referrer"
        loading="lazy"
        onLoad={() => setLoadedSrc(src)}
      />
    </span>
  );
}

export function InvalidHtmlPreview() {
  const { t } = useTranslation();
  return (
    <span className="html-preview html-preview-invalid" role="alert">
      {t("chat.invalidHtmlPreview")}
    </span>
  );
}
