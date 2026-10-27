import { act, fireEvent, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "../i18n";
import type { IssuePayload, IssueViewState } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { IssueDetail, IssueEvent } from "./types";

/// The card page under a fake layout, for the one thing that needs one: a
/// foldable closing or opening (or Split View) changes the webview's WIDTH live,
/// every block rewraps, and `scrollTop` keeps its pixel value.
///
/// jsdom lays nothing out, so the stubs below are the smallest layout that
/// makes a place mean something: the page scrolls `scrollTop`, and every
/// anchored block (`data-anchor`) is `blockH` tall, stacked in document order.
/// A width change is modelled as every block rewrapping to a new height at once.

const VIEW = 300;
let blockH = 100;
let scrollTop = 0;
let pageWidth = 390;
let observe: ((width: number) => void) | null = null;
let realRect: typeof Element.prototype.getBoundingClientRect;

const card: IssueDetail = {
  number: 7,
  project_id: "01JBOARD",
  title: "the long thread",
  description: "what this is for",
  status: "in_progress",
  priority: "none",
  position: 0,
  pinned: false,
  stage: 0,
  unread: 0,
  last_run_failed: false,
  approval_pending: false,
  opened_by_agent: false,
  created_at_ms: 1,
  updated_at_ms: 9,
};

function comment(n: number): IssueEvent {
  return {
    id: `e${String(n)}`,
    number: 7,
    actor: { kind: "agent", id: "a-dev", handle: "dev-1" },
    body: { kind: "comment", text: `comment ${String(n)}` },
    created_at_ms: n,
  };
}

const EVENTS = Array.from({ length: 12 }, (_, i) => comment(i + 1));
const PEOPLE = { "a-dev": { handle: "dev-1", monogram: "D1" } };
const PAYLOAD: IssuePayload = {
  issue: card,
  events: EVENTS,
  runs: [],
  people: PEOPLE,
  timelineLive: true,
};

function anchors(): Element[] {
  return [...document.querySelectorAll("[data-anchor]")];
}

function contentHeight(): number {
  return anchors().length * blockH;
}

function topOf(id: string): number {
  return blockIndex(id) * blockH - scrollTop;
}

function blockIndex(id: string): number {
  return anchors().findIndex((el) => el.getAttribute("data-anchor") === id);
}

function scroller(): HTMLElement {
  const el = document.querySelector<HTMLElement>(".issue-page");
  if (el === null) throw new Error("issue scroller missing");
  return el;
}

function isPage(el: Element): boolean {
  return el.classList.contains("issue-page");
}

const restore: (() => void)[] = [];

function stub<K extends keyof HTMLElement>(
  key: K,
  get: (el: HTMLElement) => unknown,
  set?: (v: number) => void,
): void {
  const original = Object.getOwnPropertyDescriptor(HTMLElement.prototype, key);
  Object.defineProperty(HTMLElement.prototype, key, {
    configurable: true,
    get(this: HTMLElement) {
      return get(this);
    },
    set(this: HTMLElement, v: number) {
      if (isPage(this)) set?.(v);
    },
  });
  restore.push(() => {
    if (original === undefined) Reflect.deleteProperty(HTMLElement.prototype, key);
    else Object.defineProperty(HTMLElement.prototype, key, original);
  });
}

beforeEach(() => {
  blockH = 100;
  scrollTop = 0;
  pageWidth = 390;
  observe = null;
  stub("scrollHeight", (el) => (isPage(el) ? contentHeight() : 0));
  stub("clientHeight", (el) => (isPage(el) ? VIEW : 0));
  stub("clientWidth", (el) => (isPage(el) ? pageWidth : 0));
  stub(
    "scrollTop",
    (el) => (isPage(el) ? scrollTop : 0),
    (v) => {
      scrollTop = Math.min(Math.max(0, v), Math.max(0, contentHeight() - VIEW));
    },
  );
  realRect = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (this: Element): DOMRect {
    const id = this.getAttribute("data-anchor");
    if (id === null) return realRect.call(this);
    const top = topOf(id);
    return { top, bottom: top + blockH, height: blockH, left: 0, right: 0, width: 0 } as DOMRect;
  };
  vi.stubGlobal(
    "ResizeObserver",
    class {
      cb: ResizeObserverCallback;
      constructor(cb: ResizeObserverCallback) {
        this.cb = cb;
      }
      observe(): void {
        observe = (width) => {
          this.cb(
            [{ contentRect: { width } } as ResizeObserverEntry],
            this as unknown as ResizeObserver,
          );
        };
      }
      unobserve(): void {}
      disconnect(): void {
        observe = null;
      }
    },
  );
  vi.spyOn(console, "log").mockImplementation(() => {});
});

afterEach(() => {
  for (const undo of restore.splice(0)) undo();
  Element.prototype.getBoundingClientRect = realRect;
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function page(initialState?: IssueViewState) {
  return render(
    <I18nextProvider i18n={i18n}>
      <IssuePage targetId="visit-resize" initialState={initialState} initialPayload={PAYLOAD} />
    </I18nextProvider>,
  );
}

/// The observer's first report, at the width the page opened at.
function settleObserver(): void {
  act(() => observe?.(pageWidth - 40));
}

function rewrap(height: number, width: number): void {
  blockH = height;
  pageWidth = width;
  act(() => observe?.(width - 40));
}

/// A reader's own scroll: a finger, then the scroll event, then the frame the
/// place is read in.
async function readerScrollsTo(top: number): Promise<void> {
  fireEvent.pointerDown(scroller());
  scrollTop = top;
  fireEvent.scroll(scroller());
  await act(async () => {
    await new Promise((r) => setTimeout(r, 50));
  });
}

function lastPost(type: string): Record<string, unknown> | undefined {
  const spy = console.log as unknown as { mock: { calls: unknown[][] } };
  return spy.mock.calls
    .filter((c) => c[0] === "[baybo bridge]")
    .map((c) => c[1] as Record<string, unknown>)
    .filter((p) => p.type === type)
    .pop();
}

describe("a width change on the card page", () => {
  it("keeps a reader mid-card on the block they were on", async () => {
    page();
    settleObserver();
    await readerScrollsTo(730);
    expect(topOf("e4")).toBe(-30);

    rewrap(180, 700);

    expect(topOf("e4")).toBe(-30);
  });

  it("keeps a reader at the newest activity there", () => {
    page();
    settleObserver();
    expect(scrollTop).toBe(contentHeight() - VIEW);

    rewrap(180, 700);

    expect(scrollTop).toBe(contentHeight() - VIEW);
    expect(lastPost("activityAtBottom")?.atBottom).toBe(true);
  });

  it("tells native where the edges are now", async () => {
    page();
    settleObserver();
    await readerScrollsTo(0);
    expect(lastPost("activityAtTop")?.atTop).toBe(true);

    // The block on screen is the head; nothing above it can move it off the top.
    rewrap(180, 700);
    expect(lastPost("activityAtTop")?.atTop).toBe(true);
    expect(lastPost("activityAtBottom")?.atBottom).toBe(false);
  });

  it("leaves the reader alone when only the height changed", async () => {
    page();
    settleObserver();
    await readerScrollsTo(730);

    act(() => observe?.(pageWidth - 40));

    expect(scrollTop).toBe(730);
  });
});

describe("a covered visit coming back", () => {
  it("saves the block on screen and the width it was read at", async () => {
    page();
    settleObserver();
    await readerScrollsTo(730);

    expect(window.issuePage?.snapshotState()).toEqual({
      scrollTop: 730,
      folds: {},
      anchor: { id: "e4", offset: -30, width: 390 },
    });
  });

  it("restores by that block when the width changed while it was covered", () => {
    blockH = 180;
    pageWidth = 700;
    page({ scrollTop: 730, folds: {}, anchor: { id: "e4", offset: -30, width: 390 } });

    expect(topOf("e4")).toBe(-30);
  });

  it("restores the saved offset as-is at the same width", () => {
    page({ scrollTop: 730, folds: {}, anchor: { id: "e4", offset: -30, width: 390 } });

    expect(scrollTop).toBe(730);
  });

  it("restores the saved offset when native did not carry an anchor back", () => {
    blockH = 180;
    pageWidth = 700;
    page({ scrollTop: 730, folds: {} });

    expect(scrollTop).toBe(730);
  });
});
