import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  anchorShift,
  captureAnchor,
  holdPlaceAcrossWidth,
  type ScrollAnchor,
} from "./resizeAnchor";

/// jsdom lays nothing out, so each row's rect is supplied: rows are stacked
/// `rowH` tall from the top of the content, and `scrolled` moves them all up.
/// That is the whole model a place needs — which row, and where its top sits.
const ATTR = "data-anchor";
let rowH = 100;
let scrolled = 0;

function rect(top: number, height: number, width = 0): DOMRect {
  const box = { top, bottom: top + height, height, left: 0, right: width, width };
  return { ...box, x: 0, y: top } as DOMRect;
}

function page(count: number): HTMLElement {
  const root = document.createElement("div");
  for (let i = 0; i < count; i++) {
    const row = document.createElement("div");
    row.setAttribute(ATTR, `r${String(i)}`);
    row.getBoundingClientRect = () => rect(i * rowH - scrolled, rowH);
    root.appendChild(row);
  }
  document.body.appendChild(root);
  return root;
}

beforeEach(() => {
  rowH = 100;
  scrolled = 0;
});

afterEach(() => {
  document.body.replaceChildren();
});

describe("captureAnchor", () => {
  it("takes the first row still on screen and where its top sits", () => {
    const root = page(10);
    scrolled = 250;
    // r0 and r1 end at or above the edge; r2 is half off the top.
    expect(captureAnchor(root, ATTR)).toEqual({ id: "r2", offset: -50 });
  });

  it("measures from a scroller's own top edge, not the viewport's", () => {
    const root = page(10);
    scrolled = 250;
    expect(captureAnchor(root, ATTR, 60)).toEqual({ id: "r3", offset: -10 });
  });

  it("holds nothing when no row is on screen", () => {
    const root = page(3);
    scrolled = 1000;
    expect(captureAnchor(root, ATTR)).toBeNull();
  });
});

describe("anchorShift", () => {
  it("is how far the scroller must move to put the row back", () => {
    const root = page(10);
    scrolled = 250;
    const anchor = captureAnchor(root, ATTR) as ScrollAnchor;
    // Every row rewraps taller: r2 now starts 400 - 250 = 150px down.
    rowH = 200;
    expect(anchorShift(root, ATTR, anchor)).toBe(200);
    scrolled += 200;
    expect(captureAnchor(root, ATTR)).toEqual(anchor);
  });

  it("is null once the row is gone", () => {
    const root = page(3);
    expect(anchorShift(root, ATTR, { id: "missing", offset: 0 })).toBeNull();
  });

  it("finds an id that is not a plain identifier", () => {
    const root = page(1);
    root.firstElementChild?.setAttribute(ATTR, 'a"b c');
    expect(anchorShift(root, ATTR, { id: 'a"b c', offset: -5 })).toBe(5);
  });
});

describe("anchoring a long row by its blocks", () => {
  const PARTS = "p";
  let paraH = 50;

  /// One row of `count` paragraphs, each `paraH` tall, starting at the top of
  /// the content — a single long reply, as the transcript has them.
  function longRow(count: number): HTMLElement {
    const root = document.createElement("div");
    const row = document.createElement("div");
    row.setAttribute(ATTR, "reply");
    row.getBoundingClientRect = () => rect(-scrolled, count * paraH);
    for (let i = 0; i < count; i++) {
      const p = document.createElement("p");
      p.getBoundingClientRect = () => rect(i * paraH - scrolled, paraH);
      row.appendChild(p);
    }
    root.appendChild(row);
    document.body.appendChild(root);
    return root;
  }

  beforeEach(() => {
    paraH = 50;
  });

  it("refines a row that straddles the edge to the block holding it", () => {
    const root = longRow(20);
    scrolled = 420;
    // p8 spans 400..450: 20px of it is still on screen.
    expect(captureAnchor(root, ATTR, 0, PARTS)).toEqual({
      id: "reply",
      offset: -420,
      part: { index: 8, offset: -20 },
    });
  });

  it("restores by the block after every line rewrapped", () => {
    const root = longRow(20);
    scrolled = 420;
    const anchor = captureAnchor(root, ATTR, 0, PARTS) as ScrollAnchor;
    // A narrower window: every paragraph wraps to twice the height. The row's
    // own offset would put p4 at the edge; the block puts p8 back there.
    paraH = 100;
    expect(anchorShift(root, ATTR, anchor, 0, PARTS)).toBe(800 - 420 + 20);
  });

  it("leaves a row that starts on screen to its own offset", () => {
    const root = longRow(20);
    scrolled = -30;
    expect(captureAnchor(root, ATTR, 0, PARTS)).toEqual({ id: "reply", offset: 30 });
  });

  it("passes a row whose only on-screen part is its tail to the next row", () => {
    const root = longRow(4);
    const reply = root.firstElementChild as HTMLElement;
    reply.getBoundingClientRect = () => rect(-scrolled, 230);
    const next = document.createElement("div");
    next.setAttribute(ATTR, "next");
    next.getBoundingClientRect = () => rect(230 - scrolled, 40);
    root.appendChild(next);
    // The reply's paragraphs end at 200; its 30px tail (a timestamp) is all
    // that is left on screen, and the next row starts 20px below the edge.
    scrolled = 210;
    expect(captureAnchor(root, ATTR, 0, PARTS)).toEqual({ id: "next", offset: 20 });
  });

  it("falls back to the row when the block is gone", () => {
    const root = longRow(3);
    const anchor: ScrollAnchor = { id: "reply", offset: -30, part: { index: 9, offset: -5 } };
    expect(anchorShift(root, ATTR, anchor, 0, PARTS)).toBe(30);
  });
});

describe("holdPlaceAcrossWidth", () => {
  let observed: ((width: number) => void) | null = null;
  let frames: FrameRequestCallback[] = [];
  let boxWidth = 400;

  function resize(width: number): void {
    observed?.(width);
  }

  function frame(): void {
    const due = frames;
    frames = [];
    for (const cb of due) cb(0);
  }

  beforeEach(() => {
    observed = null;
    frames = [];
    boxWidth = 400;
    vi.stubGlobal(
      "ResizeObserver",
      class {
        cb: ResizeObserverCallback;
        constructor(cb: ResizeObserverCallback) {
          this.cb = cb;
        }
        observe(): void {
          observed = (width) => {
            this.cb(
              [{ contentRect: { width } } as ResizeObserverEntry],
              this as unknown as ResizeObserver,
            );
          };
        }
        unobserve(): void {}
        disconnect(): void {
          observed = null;
        }
      },
    );
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => frames.push(cb));
    vi.stubGlobal("cancelAnimationFrame", () => {
      frames = [];
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  function hold(root: HTMLElement) {
    const box = document.createElement("div");
    box.getBoundingClientRect = () => rect(0, 0, boxWidth);
    const scroller = new EventTarget();
    const seen: { widthChanged: boolean; before: ScrollAnchor | null }[] = [];
    const disconnect = holdPlaceAcrossWidth(
      box,
      scroller,
      () => captureAnchor(root, ATTR),
      (resize) => {
        seen.push(resize);
        // What a page does with it: put the row back.
        if (!resize.widthChanged || resize.before === null) return;
        scrolled += anchorShift(root, ATTR, resize.before) ?? 0;
      },
    );
    const scroll = (to: number) => {
      scrolled = to;
      scroller.dispatchEvent(new Event("scroll"));
    };
    return { seen, scroll, disconnect };
  }

  it("reports the first observation as no width change", () => {
    const { seen } = hold(page(10));
    resize(400);
    expect(seen).toEqual([{ widthChanged: false, before: { id: "r0", offset: 0 } }]);
  });

  it("hands a width change the place from BEFORE the reflow", () => {
    const { seen, scroll } = hold(page(10));
    resize(400);
    scroll(250);
    frame();

    // The observer only runs after layout, when every row has already rewrapped.
    rowH = 200;
    resize(300);

    expect(seen[1]).toEqual({ widthChanged: true, before: { id: "r2", offset: -50 } });
  });

  it("captures once per frame however many scroll events land in it", () => {
    const { scroll } = hold(page(10));
    resize(400);
    scroll(100);
    scroll(150);
    scroll(250);
    expect(frames).toHaveLength(1);
  });

  // A scroll event can land in the very frame the viewport changed (WebKit
  // clamping scrollTop to the new extent). Reading the place there forces the
  // new layout, so it would hand the resize a place that is already wrong.
  it("skips a scroll capture taken in the frame the box changed width", () => {
    const { seen, scroll } = hold(page(10));
    resize(400);
    scroll(250);
    frame();

    boxWidth = 300;
    rowH = 200;
    scroll(260);
    frame();
    resize(280);

    expect(seen[1].before).toEqual({ id: "r2", offset: -50 });
  });

  it("captures again after a skipped frame, even with no observation between", () => {
    const { seen, scroll } = hold(page(10));
    resize(400);
    boxWidth = 900;
    scroll(250);
    frame();
    scroll(260);
    frame();
    resize(400);
    // Content width unchanged — a capped reading band — so no reflow, and the
    // scroll the reader made since is the place.
    expect(seen[1]).toEqual({ widthChanged: false, before: { id: "r2", offset: -60 } });
  });

  it("re-captures after every resize, so consecutive changes chain", () => {
    const root = page(10);
    const { seen, scroll } = hold(root);
    resize(400);
    scroll(230);
    frame();
    rowH = 200;
    resize(300);
    rowH = 50;
    resize(200);
    expect(seen[2].before).toEqual({ id: "r2", offset: -30 });
    expect(captureAnchor(root, ATTR)).toEqual({ id: "r2", offset: -30 });
  });

  it("stops listening on disconnect", () => {
    const { seen, scroll, disconnect } = hold(page(10));
    resize(400);
    disconnect();
    scroll(250);
    expect(frames).toHaveLength(0);
    resize(300);
    expect(seen).toHaveLength(1);
  });
});
