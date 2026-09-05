import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { I18nextProvider } from "react-i18next";

const { postMaximized } = vi.hoisted(() => ({
  postMaximized: vi.fn(),
}));
vi.mock("./bridge", () => ({
  postHtmlPreviewMaximized: postMaximized,
}));

import i18n from "./i18n";
import { HtmlPreview, InvalidHtmlPreview } from "./HtmlPreview";
import {
  HTML_PREVIEW_COLLAPSE_EVENT,
  HTML_PREVIEW_DRAG_BEGIN_EVENT,
  HTML_PREVIEW_DRAG_END_EVENT,
  HTML_PREVIEW_DRAG_MOVE_EVENT,
  HTML_PREVIEW_MAXIMIZED_CLASS,
  htmlPreviewBlobId,
} from "./htmlPreviewProtocol";

const BLOB_ID = `sha256:${"a".repeat(64)}.${"b".repeat(32)}`;

function renderPreview(blobId = BLOB_ID) {
  return render(
    <I18nextProvider i18n={i18n}>
      <HtmlPreview blobId={blobId} />
    </I18nextProvider>,
  );
}

async function maximize(): Promise<HTMLElement> {
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: "Show HTML preview full screen" }));
  const box = screen.getByTitle("Agent-created HTML preview").closest(".html-preview");
  expect(box).not.toBeNull();
  return box as HTMLElement;
}

/// Wrapped in `act` because the component answers these on a window listener,
/// outside React's own event plumbing — without it the render that retires the
/// fullscreen class is still queued when the assertion reads the DOM.
function fire(event: Event): void {
  act(() => {
    window.dispatchEvent(event);
  });
}

/// jsdom lays nothing out, so the drag's two ends have to be supplied: a
/// 400x800 screen and a 360x420 slot at (20, 300), on a 400pt-wide viewport.
/// A quarter-screen drag then lands on numbers a human can check by hand.
function stubGeometry(box: HTMLElement): void {
  const slot = box.parentElement;
  if (slot === null) throw new Error("the preview must sit in its <pre> slot");
  const rect = (left: number, top: number, width: number, height: number) =>
    ({ left, top, width, height, right: left + width, bottom: top + height }) as DOMRect;
  box.getBoundingClientRect = () => rect(0, 0, 400, 800);
  slot.getBoundingClientRect = () => rect(20, 300, 360, 420);
  window.innerWidth = 400;
}

/// Stand in for the CSS transitions the morph starts. Returns one resolver per
/// animation, so a test can land them ONE AT A TIME — which is the whole point:
/// the box must not retire until the last one is done.
function stubAnimations(box: HTMLElement, count: number): (() => void)[] {
  const resolvers: (() => void)[] = [];
  const animations = Array.from({ length: count }, () => {
    let settle = (): void => undefined;
    const finished = new Promise<void>((resolve) => {
      settle = resolve;
    });
    resolvers.push(() => {
      settle();
    });
    return { finished } as unknown as Animation;
  });
  box.getAnimations = () => animations;
  return resolvers;
}

function drag(px: number, dismiss: boolean): void {
  fire(new Event(HTML_PREVIEW_DRAG_BEGIN_EVENT));
  fire(new CustomEvent(HTML_PREVIEW_DRAG_MOVE_EVENT, { detail: px }));
  fire(new CustomEvent(HTML_PREVIEW_DRAG_END_EVENT, { detail: dismiss }));
}

// The unmount of a still-maximized preview hands native chrome back during the
// PREVIOUS test's cleanup, which would otherwise be counted here.
beforeEach(() => {
  postMaximized.mockClear();
});

describe("HTML preview marker", () => {
  it("accepts exactly one capability blob id in a baybo-html fence", () => {
    expect(htmlPreviewBlobId("language-baybo-html", `\n${BLOB_ID}\n`)).toBe(BLOB_ID);
    expect(htmlPreviewBlobId("language-html", BLOB_ID)).toBeNull();
    expect(htmlPreviewBlobId("language-baybo-html", "<html>raw</html>")).toBe("");
    expect(htmlPreviewBlobId("language-baybo-html", `${BLOB_ID}\nextra`)).toBe("");
  });
});

describe("HtmlPreview", () => {
  it("loads the blob route in a script-only opaque-origin sandbox", () => {
    renderPreview();
    const frame = screen.getByTitle("Agent-created HTML preview");
    expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    expect(frame.getAttribute("sandbox")).not.toContain("allow-same-origin");
    expect(frame).toHaveAttribute("loading", "lazy");
    expect(frame).toHaveAttribute(
      "src",
      `/html-preview/${BLOB_ID}?reload=0`,
    );
  });

  it("maximizes the same iframe and reports native chrome state", async () => {
    const user = userEvent.setup();
    renderPreview();
    const frame = screen.getByTitle("Agent-created HTML preview");

    const box = await maximize();
    stubGeometry(box);
    const landed = stubAnimations(box, 2);
    expect(screen.getByTitle("Agent-created HTML preview")).toBe(frame);
    expect(box).toHaveClass("is-maximized");
    expect(document.documentElement).toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS);
    expect(postMaximized).toHaveBeenLastCalledWith(true);
    // The morph's far end, written as viewport units rather than a measured
    // pixel count so a rotation stays correct with nobody re-measuring.
    expect(box.style.width).toBe("100vw");
    expect(box.style.height).toBe("100dvh");

    await user.click(screen.getByRole("button", { name: "Close full-screen HTML preview" }));
    // Native chrome is handed back the instant the dismissal starts — the box is
    // still covering the screen while it animates away.
    expect(postMaximized).toHaveBeenLastCalledWith(false);
    expect(screen.getByTitle("Agent-created HTML preview")).toBe(frame);
    // It travels back into its slot rather than fading: a fade would uncover
    // the EMPTY reserved slot, which is what the blank frames at the end of
    // every dismissal used to be.
    expect(box.style.width).not.toBe("100vw");
    expect(box).toHaveClass("is-maximized");

    // The morph moves seven properties and `transitionend` fires per property.
    // Retiring on the first one left the rest a pixel or two short, and the
    // class flip snapped them — the flicker after a shrink. One landing is not
    // the morph landing.
    landed[0]();
    await Promise.resolve();
    expect(box).toHaveClass("is-maximized");

    landed[1]();
    await waitFor(() =>
      expect(document.documentElement).not.toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS),
    );
    expect(box).not.toHaveClass("is-maximized");
  });

  it("shrinks toward its slot as the native edge swipe travels", async () => {
    renderPreview();
    const box = await maximize();
    stubGeometry(box);
    const landed = stubAnimations(box, 1);
    postMaximized.mockClear();

    fire(new Event(HTML_PREVIEW_DRAG_BEGIN_EVENT));
    // A quarter of the way across the screen is a quarter of the way home. The
    // box SHRINKS toward the slot — it does not slide, which would drag a
    // full-screen page sideways and read as the conversation itself moving.
    fire(new CustomEvent(HTML_PREVIEW_DRAG_MOVE_EVENT, { detail: 100 }));
    expect(box.style.transform).toBe("");
    expect(box.style.width).toBe("390px");
    expect(box.style.height).toBe("705px");
    expect(box.style.left).toBe("5px");
    expect(box.style.top).toBe("75px");

    // Native clamps at the edge, but a negative that slipped through must not
    // push the box PAST full screen.
    fire(new CustomEvent(HTML_PREVIEW_DRAG_MOVE_EVENT, { detail: -40 }));
    expect(box.style.width).toBe("400px");
    expect(box.style.height).toBe("800px");

    fire(new CustomEvent(HTML_PREVIEW_DRAG_END_EVENT, { detail: true }));
    expect(postMaximized).toHaveBeenLastCalledWith(false);
    landed[0]();
    await waitFor(() =>
      expect(document.documentElement).not.toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS),
    );
    // Every inline style the drag wrote is handed back, so the next open starts
    // from the stylesheet rather than mid-swipe.
    expect(box.style.width).toBe("");
    expect(box.style.transition).toBe("");
  });

  /// Leaving `position: fixed` repaints every glyph in the box, and that repaint
  /// has to land WITH the motion. A release that follows a long drag has almost
  /// nothing left to travel: animating it anyway parked the box for a quarter
  /// second and THEN flashed — which is what the flicker after a shrink was.
  it("retires a box that is already home instead of animating nothing", async () => {
    const user = userEvent.setup();
    renderPreview();
    const box = await maximize();
    // No stub: jsdom lays nothing out, so the box and its slot report the same
    // (zero) rect — the "there is no travel left" case exactly.
    postMaximized.mockClear();

    await user.click(screen.getByRole("button", { name: "Close full-screen HTML preview" }));

    expect(postMaximized).toHaveBeenLastCalledWith(false);
    expect(box).not.toHaveClass("is-maximized");
    expect(document.documentElement).not.toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS);
    expect(box.style.cssText).toBe("");
  });

  it("returns to full screen when the swipe is released short", async () => {
    renderPreview();
    const box = await maximize();
    stubGeometry(box);
    postMaximized.mockClear();

    drag(40, false);
    expect(box.style.width).toBe("100vw");
    expect(box.style.height).toBe("100dvh");
    expect(postMaximized).not.toHaveBeenCalled();
    expect(document.documentElement).toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS);
    expect(box).toHaveClass("is-maximized");
  });

  it("ignores an edge swipe while nothing is full screen", () => {
    renderPreview();
    const box = screen.getByTitle("Agent-created HTML preview").closest(".html-preview");
    drag(300, true);
    expect(postMaximized).not.toHaveBeenCalled();
    expect(box).not.toHaveClass("is-maximized");
  });

  it("collapses without animating when native detaches the transcript", async () => {
    renderPreview();
    const box = await maximize();

    fire(new Event(HTML_PREVIEW_COLLAPSE_EVENT));

    // No frame of grace: the page is about to be repointed at another session.
    expect(box).not.toHaveClass("is-maximized");
    expect(document.documentElement).not.toHaveClass(HTML_PREVIEW_MAXIMIZED_CLASS);
    expect(postMaximized).toHaveBeenLastCalledWith(false);
  });

  it("renders an explicit error instead of navigating for a bad marker", () => {
    render(
      <I18nextProvider i18n={i18n}>
        <InvalidHtmlPreview />
      </I18nextProvider>,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("Invalid HTML preview blob id");
    expect(screen.queryByTitle("Agent-created HTML preview")).toBeNull();
  });
});
