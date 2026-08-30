import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { Transcript } from "./Transcript";
import type { PersistedState, Row, TranscriptRowItem } from "./types";

/// The scroll model, under a fake layout.
///
/// Every other suite here exercises pure reducers because jsdom has no layout —
/// `scrollHeight` is 0, `getBoundingClientRect` is all zeros, and every
/// follow/pin/anchor branch in `<Transcript>` degenerates to a no-op. So the one
/// half of this component that is nothing BUT those branches had no test at all,
/// and the regression it now pins is what that costs: a `sync_page` REPLACE
/// landing on a reader parked up in history threw away the pages they had
/// scrolled up for and slammed them to the newest edge.
///
/// The stubs below are the smallest layout that makes those branches mean
/// something: the document scrolls, every row of the log is `ROW_H` tall, and a
/// row's rect follows from its index. That is enough for the arithmetic under
/// test — `prevScrollTop + (scrollHeight - prevScrollHeight)` for a prepend, and
/// `top - anchor.top` for a REPLACE — to be exercised for real. It is NOT a
/// WKWebView: momentum, rubber-band overscroll and the UI-process scroll thread
/// are all outside what this can say anything about.

const VIEW = 800;
const ROW_H = 120;

let scrollTop = 0;
let roCallbacks: (() => void)[] = [];
let realRect: typeof Element.prototype.getBoundingClientRect;

function logEl(): Element | null {
  return document.querySelector(".chat-log");
}

/// Every direct child of `.chat-log` — rows, the paging spinner, the "load
/// earlier" button — is one `ROW_H` band, so the document's height and any row's
/// position both fall out of the child list.
function rowIndex(el: Element): number {
  const log = logEl();
  return log === null ? -1 : Array.prototype.indexOf.call(log.children, el);
}

function contentHeight(): number {
  const log = logEl();
  return log === null ? VIEW : Math.max(VIEW, log.children.length * ROW_H);
}

function maxScroll(): number {
  return Math.max(0, contentHeight() - VIEW);
}

function rowIds(): string[] {
  return [...document.querySelectorAll("[data-row-id]")].map(
    (el) => el.getAttribute("data-row-id") ?? "",
  );
}

/// The row under the top of the viewport — what `captureRowAnchor` reads, and
/// what a reader parked in history must still be looking at afterwards.
function rowAtViewportTop(): string | null {
  for (const el of document.querySelectorAll("[data-row-id]")) {
    if (el.getBoundingClientRect().bottom > 0) return el.getAttribute("data-row-id");
  }
  return null;
}

function installLayout(): void {
  scrollTop = 0;
  // jsdom leaves `scrollingElement` null in its default (quirks) document, which
  // alone makes every scroll op in the component a no-op.
  Object.defineProperty(document, "scrollingElement", {
    configurable: true,
    get: () => document.documentElement,
  });
  Object.defineProperty(document.documentElement, "scrollHeight", {
    configurable: true,
    get: () => contentHeight(),
  });
  Object.defineProperty(document.documentElement, "clientHeight", {
    configurable: true,
    get: () => VIEW,
  });
  Object.defineProperty(document.documentElement, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      const next = Math.min(Math.max(0, v), maxScroll());
      if (next === scrollTop) return;
      scrollTop = next;
      window.dispatchEvent(new Event("scroll"));
    },
  });
  // jsdom ships no `scrollIntoView` at all, so a jump throws rather than
  // scrolling. Model it the way the fake layout already models everything else:
  // a row's position IS its index, so parking one at the top of the viewport is
  // a scrollTop write — which also makes the landing assertable.
  Element.prototype.scrollIntoView = function (this: Element): void {
    const i = rowIndex(this);
    if (i < 0) return;
    document.documentElement.scrollTop = i * ROW_H;
  };
  realRect = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (this: Element): DOMRect {
    const i = rowIndex(this);
    if (i < 0) return realRect.call(this);
    const top = i * ROW_H - scrollTop;
    return {
      top,
      bottom: top + ROW_H,
      height: ROW_H,
      left: 0,
      right: 0,
      width: 0,
      x: 0,
      y: top,
      toJSON: () => ({}),
    } as DOMRect;
  };
}

/// The real ResizeObserver fires after layout on every `.chat-log` growth, and
/// its callback is one of the writes that can slam the log to the bottom — so
/// the tests drive it by hand rather than stubbing it inert.
function installObservers(): void {
  roCallbacks = [];
  vi.stubGlobal(
    "ResizeObserver",
    class {
      cb: () => void;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(): void {
        roCallbacks.push(this.cb);
      }
      unobserve(): void {}
      disconnect(): void {
        roCallbacks = roCallbacks.filter((c) => c !== this.cb);
      }
    },
  );
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      observe(): void {}
      unobserve(): void {}
      disconnect(): void {}
    },
  );
}

function row(ordinal: number): Row {
  return {
    id: `m${ordinal}`,
    role: ordinal % 2 === 0 ? "user" : "assistant",
    content: `row ${ordinal}`,
    ordinal,
  };
}

function item(ordinal: number): TranscriptRowItem {
  return {
    id: `m${ordinal}`,
    ordinal,
    kind: "message",
    role: ordinal % 2 === 0 ? "user" : "assistant",
    text: `row ${ordinal}`,
    created_at: "2026-08-01T00:00:00Z",
  };
}

function items(from: number, through: number): TranscriptRowItem[] {
  const out: TranscriptRowItem[] = [];
  for (let o = from; o <= through; o++) out.push(item(o));
  return out;
}

/// A mirror of `count` rows ending at `newest`, with older pages still on the
/// server — the shape every long conversation restores in. `cursor` is the
/// persisted sync watermark: pass it below `newest` to restore a thread whose
/// rows OUTRAN the cursor (which is what makes the next baseline a repair), or
/// `null` for a thread with no watermark at all.
function mirror(count: number, newest: number, cursor: number | null = newest): PersistedState {
  const rows: Row[] = [];
  for (let i = count - 1; i >= 0; i--) rows.push(row(newest - i));
  return {
    messages: rows,
    lastOrdinal: cursor,
    oldestOrdinal: newest - count + 1,
    hasMoreOlder: true,
  };
}

/// Let the mount effects, the deferred-head drain (two rAFs) and any pending
/// commit land, then run the ResizeObserver the way a real growth would.
async function settle(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 60));
  });
  act(() => {
    for (const cb of [...roCallbacks]) cb();
  });
  await act(async () => {
    await new Promise((r) => setTimeout(r, 10));
  });
}

async function open(count: number, newest: number, cursor?: number | null): Promise<void> {
  render(
    <I18nextProvider i18n={i18n}>
      <Transcript restored={mirror(count, newest, cursor)} initialConnEpoch={0} />
    </I18nextProvider>,
  );
  await settle();
}

async function pushFrame(frame: Record<string, unknown>): Promise<void> {
  await act(async () => {
    window.baybo.pushFrame(JSON.stringify(frame));
  });
  await settle();
}

/// Model a USER scroll: real readers move the viewport with a finger, and
/// onScroll now tells fingers from drift (an off-edge reading with no recent
/// touch re-pins — see the drift branch). A bare scrollTop write with no touch
/// would read as drift and get pinned right back.
async function scrollTo(top: number): Promise<void> {
  await act(async () => {
    window.dispatchEvent(new Event("touchstart"));
    document.documentElement.scrollTop = top;
    window.dispatchEvent(new Event("touchend"));
  });
  await settle();
}

/// Outbound bridge posts. Without `window.webkit` they degrade to console.log,
/// which is what the dev browser relies on too.
function posts(): Record<string, unknown>[] {
  const spy = console.log as unknown as { mock: { calls: unknown[][] } };
  return spy.mock.calls
    .filter((c) => c[0] === "[baybo bridge]")
    .map((c) => c[1] as Record<string, unknown>);
}

beforeEach(() => {
  installLayout();
  installObservers();
  vi.spyOn(console, "log").mockImplementation(() => {});
});

afterEach(() => {
  Element.prototype.getBoundingClientRect = realRect;
});

describe("scroll-up paging holds the viewport", () => {
  it("opens a restored thread on its newest rows", async () => {
    await open(60, 999);
    expect(scrollTop).toBe(maxScroll());
  });

  // The real-WebKit cold-open race: the drained head's markdown keeps
  // reflowing after the drain's pin wrote, so the pin's own scroll event reads
  // an off-edge gap. Un-glided, onScroll took that reading at face value —
  // follow disarmed, jump button up, and every later growth left the reader
  // further off the bottom (the "cold open no longer lands at the bottom"
  // regression). The drain pin now arms the glide window, so the transient
  // reading is held and the ResizeObserver pin chases the edge as it settles.
  it("holds the newest edge when layout drifts out from under the drain's pin", async () => {
    await open(60, 999);
    expect(scrollTop).toBe(maxScroll());
    await act(async () => {
      // Bypass the setter: this models the EDGE moving, not the reader.
      scrollTop = maxScroll() - 300;
      window.dispatchEvent(new Event("scroll"));
      roCallbacks.forEach((cb) => cb());
    });
    await settle();
    expect(scrollTop).toBe(maxScroll());
    expect(posts().some((p) => p.type === "jumpVisible" && p.visible === true)).toBe(false);
  });

  it("keeps the rows under the reader in place when an older page prepends", async () => {
    await open(60, 999);
    await scrollTo(0);
    expect(posts().some((p) => p.type === "fetchHistory")).toBe(true);

    const before = contentHeight();
    await pushFrame({
      kind: "history_page",
      rows: items(890, 939),
      oldest_ordinal: 890,
      has_more: true,
    });

    // Exactly the height the page added — the reader is looking at the same row.
    expect(scrollTop).toBe(contentHeight() - before);
    expect(scrollTop).toBeLessThan(maxScroll());
  });

  it("holds through a finger that stays down for the whole round trip", async () => {
    await open(60, 999);
    act(() => {
      window.dispatchEvent(new Event("touchstart"));
    });
    await scrollTo(0);
    await pushFrame({
      kind: "history_page",
      rows: items(890, 939),
      oldest_ordinal: 890,
      has_more: true,
    });
    const held = scrollTop;
    act(() => {
      window.dispatchEvent(new Event("touchend"));
    });
    await settle();
    // The lift must not read the prepend's coordinate shift as "content grew
    // under a reader sitting at the bottom" and catch up to the newest edge.
    expect(scrollTop).toBe(held);
  });
});

/// The turn indicator's box, under a reader sitting at the newest edge.
///
/// `turnActive` is server-driven — it arrives a round trip after the send — so
/// mounting the indicator on it inserted a row a beat AFTER the user's bubble
/// had settled, and the follow pin teleported the thread up for it: a second,
/// unprompted lurch (43px in the real layout; one whole `ROW_H` band here).
/// The box is now claimed by the send itself, in the same commit as the bubble.
describe("the post-send working indicator", () => {
  async function userSent(msgId: string): Promise<void> {
    await act(async () => {
      window.baybo.userSent({ msgId, text: "hi", attachments: [] });
    });
    await settle();
  }

  function pending(): { total: number; reserved: number } {
    return {
      total: document.querySelectorAll(".work-pending").length,
      reserved: document.querySelectorAll(".work-pending.reserved").length,
    };
  }

  it("claims its box with the send, so the turn starting moves nothing", async () => {
    await open(60, 999);
    await userSent("pm-1");
    const settled = scrollTop;

    await pushFrame({ kind: "turn_state", active: true });

    expect(scrollTop).toBe(settled);
  });

  it("holds the claimed box invisible until the turn actually starts", async () => {
    await open(60, 999);
    await userSent("pm-1");
    expect(pending()).toEqual({ total: 1, reserved: 1 });

    await pushFrame({ kind: "turn_state", active: true });

    expect(pending()).toEqual({ total: 1, reserved: 0 });
  });

  it("still shows for a turn this device did not start", async () => {
    await open(60, 999);
    await pushFrame({ kind: "turn_state", active: true });
    expect(pending()).toEqual({ total: 1, reserved: 0 });
  });

  it("retires the reserved box with the send that failed", async () => {
    await open(60, 999);
    await userSent("pm-1");
    await act(async () => {
      window.baybo.sendFailed("pm-1");
    });
    await settle();
    // No turn is coming, so the box must go with the stop button — a reserved
    // slot left behind would hold 43px of dead paper for the rest of the thread.
    expect(pending()).toEqual({ total: 0, reserved: 0 });
  });

  it("hands off to the live work block without a second reflow", async () => {
    await open(60, 999);
    await userSent("pm-1");
    await pushFrame({ kind: "turn_state", active: true });
    const settled = scrollTop;

    await pushFrame({ kind: "tool_started", call_id: "t1", tool: "read_file", label: "Read a.rs" });

    // Both are one line of the same 0.75rem chrome, so the swap is 0px in the
    // real layout — and one row in, one row out here.
    expect(pending()).toEqual({ total: 0, reserved: 0 });
    expect(scrollTop).toBe(settled);
  });
});

describe("a REPLACE under a reader parked in history", () => {
  /// Open, page one slice of history in, and park mid-thread — the state the
  /// reported bug fired from.
  async function readingHistory(cursor?: number | null): Promise<void> {
    await open(60, 999, cursor);
    await scrollTo(0);
    await pushFrame({
      kind: "history_page",
      rows: items(890, 939),
      oldest_ordinal: 890,
      has_more: true,
    });
    await scrollTo(ROW_H * 8);
  }

  const rebase = (rows: TranscriptRowItem[], oldest: number) => ({
    kind: "sync_page",
    rows,
    since_ordinal: 999,
    next_cursor: 999,
    rebased: true,
    oldest_ordinal: oldest,
    has_more_older: true,
    compaction_points: [],
  });

  // The scar. `rebased` says only that the difference outran the server's limit
  // — an agentic turn writes hundreds of invisible tool rows, so the scan bound
  // (`SYNC_SCAN_BOUND_MULTIPLIER`) is reached by ordinary use — and here is the
  // newest page instead. Ordinals are not rewritten, so the pages the reader
  // scrolled up for are still true.
  it("keeps the history the reader paged in", async () => {
    await readingHistory();
    const before = rowIds();
    expect(before).toContain("m890");

    await pushFrame(rebase(items(950, 999), 950));

    expect(rowIds()).toEqual(before);
  });

  it("leaves them looking at the same row", async () => {
    await readingHistory();
    const anchor = rowAtViewportTop();
    const at = scrollTop;
    expect(anchor).not.toBeNull();

    await pushFrame(rebase(items(950, 999), 950));

    expect(rowAtViewportTop()).toBe(anchor);
    expect(scrollTop).toBe(at);
    expect(scrollTop).toBeLessThan(maxScroll());
  });

  it("keeps the paging floor, so the next scroll-up asks for the right page", async () => {
    await readingHistory();
    await pushFrame(rebase(items(950, 999), 950));
    await scrollTo(0);

    const fetches = posts().filter((p) => p.type === "fetchHistory");
    expect(fetches[fetches.length - 1]?.beforeOrdinal).toBe(890);
  });

  // The other half of the rule: a reader who never left the newest edge still
  // gets snapped back to it, because that is where they were.
  it("still snaps a reader at the newest edge", async () => {
    await open(60, 999);
    expect(scrollTop).toBe(maxScroll());
    await pushFrame(rebase(items(950, 999), 950));
    expect(scrollTop).toBe(maxScroll());
  });

  const baseline = (rows: TranscriptRowItem[], oldest: number) => ({
    kind: "sync_page",
    rows,
    since_ordinal: null,
    next_cursor: 999,
    rebased: false,
    oldest_ordinal: oldest,
    has_more_older: true,
    compaction_points: [],
  });

  // A cursor-less baseline is a fresh install, or the `restoredUntimedWork`
  // demotion asking the gateway to re-time ONE block. It says nothing against
  // the rows on screen — which is why it must not throw them away. Before this
  // rule it did, once per open, for as long as the mirror held an untimed block.
  it("a cursor-less baseline keeps the history and the reader's place", async () => {
    await readingHistory(null);
    const before = rowIds();
    const anchor = rowAtViewportTop();
    const at = scrollTop;

    await pushFrame(baseline(items(950, 999), 950));

    expect(rowIds()).toEqual(before);
    expect(rowAtViewportTop()).toBe(anchor);
    expect(scrollTop).toBe(at);
  });

  // A REPAIR is the other one: `syncSince` refused a non-null cursor because a
  // rendered row outran it, so the thread may be out of ORDER and the rows it
  // drops are exactly the ones under suspicion.
  it("a repair baseline rebuilds the thread and lands on the newest edge", async () => {
    // Rows up to 999 restored under a cursor of 990 — the shape `syncSince`
    // refuses, so the mount sync goes out as a repair.
    await readingHistory(990);
    expect(rowIds().length).toBeGreaterThan(50);

    await pushFrame(baseline(items(950, 999), 950));

    expect(rowIds()).toEqual(items(950, 999).map((r) => r.id));
    expect(scrollTop).toBe(maxScroll());
  });

  // The cost of keeping the head: the page re-cuts a turn the head already
  // holds. A block cut at its START is flushed `turn_complete: true` — the
  // accumulator only ever learns about a block's END — so the kept half and the
  // page's half both claim to be whole turns, `sameContinuingTurn` declines, and
  // one turn renders as two "Worked" cards. It is sticky: the pair persists into
  // the mirror and `sanitizeRestoredRows` will not heal a head that says
  // complete, so it comes back on every open.
  describe("and the page re-cuts a turn the head still holds", () => {
    const workRow = (id: string, callId: string): Row => ({
      id,
      role: "work",
      steps: [{ kind: "tool", callId, label: `Bash(${callId})`, status: "ok" }],
      active: false,
      startedAt: 1_700_000_000_000,
      elapsedMs: 4_000,
      turnComplete: true,
    });

    const workItem = (id: string, callId: string): TranscriptRowItem => ({
      id,
      ordinal: Number(id.slice(1)),
      kind: "work",
      steps: [{ kind: "tool", call_id: callId, tool: "bash", tool_label: `Bash(${callId})`, tool_status: "ok" }],
      work_started_at: "2026-08-09T10:00:02.000Z",
      work_ended_at: "2026-08-09T10:00:06.000Z",
      turn_complete: true,
    });

    /// A mirror whose newest rows END on the turn's block, with the answer that
    /// closed it at an ordinal the page's window will claim.
    const straddling: PersistedState = {
      messages: [row(940), row(942), row(944), workRow("w946", "c1"), row(956)],
      lastOrdinal: null,
      oldestOrdinal: 940,
      hasMoreOlder: true,
    };

    async function openStraddling(): Promise<void> {
      render(
        <I18nextProvider i18n={i18n}>
          <Transcript restored={straddling} initialConnEpoch={0} />
        </I18nextProvider>,
      );
      await settle();
    }

    it("joins the two halves into one card instead of stacking a second", async () => {
      await openStraddling();
      expect(document.querySelectorAll(".work-ladder")).toHaveLength(1);

      await pushFrame(baseline([workItem("w950", "c2"), item(956)], 950));

      expect(document.querySelectorAll(".work-ladder")).toHaveLength(1);
      // The head's key survives, so the card re-times in place rather than
      // remounting, and it carries both halves' steps.
      expect(rowIds()).toEqual(["m940", "m942", "m944", "w946", "m956"]);
    });

    it("still refuses across a compaction watermark — those halves are two turns", async () => {
      await openStraddling();
      await pushFrame({
        ...baseline([workItem("w950", "c2"), item(956)], 950),
        compaction_points: [{ ordinal: 948, at: "2026-08-09T10:00:04.000Z" }],
      });

      expect(document.querySelectorAll(".work-ladder")).toHaveLength(2);
    });
  });
});

/// Jump-to-ordinal — the search-hit landing.
///
/// The window has no `hasMoreNewer` and every live frame appends to the end, so
/// reaching an old row can only ever mean paging BACKWARD until it is covered.
/// That loop is reply-driven (one `requestHistory` may be in flight at a time),
/// which makes its termination the whole risk: `prependOlder` advances the floor
/// only on a NON-EMPTY page while `hasMoreOlder` can stay true, so "the floor
/// moved" is not a condition anything may rely on.
describe("jump to a search hit's ordinal", () => {
  function fetches(): number[] {
    return posts()
      .filter((p) => p.type === "fetchHistory")
      .map((p) => p.beforeOrdinal as number);
  }

  async function jumpTo(ordinal: number): Promise<void> {
    await act(async () => {
      window.baybo.jumpToOrdinal(ordinal);
    });
    await settle();
  }

  function ringedRowId(): string | null {
    const ring = document.querySelector(".jump-ring");
    return ring?.closest("[data-row-id]")?.getAttribute("data-row-id") ?? null;
  }

  it("jumps straight to a row already loaded, without asking for a page", async () => {
    await open(60, 999);
    await jumpTo(970);

    expect(fetches()).toEqual([]);
    expect(ringedRowId()).toBe("m970");
  });

  // The trap a naive port walks into. A user row is keyed by its
  // `platform_msg_id`, NOT `m<ordinal>` — the ordinal rides beside the id — so
  // resolving a hit by building `m${ordinal}` finds agent rows and silently
  // misses every user-authored one, which is most of what people search for.
  it("resolves a USER row by its ordinal, not by a reconstructed row id", async () => {
    await open(60, 999);
    await jumpTo(930);

    await pushFrame({
      kind: "history_page",
      rows: [
        {
          id: "pm-abc",
          ordinal: 930,
          kind: "message",
          role: "user",
          text: "数据库迁移怎么做",
          platform_msg_id: "pm-abc",
          created_at: "2026-08-01T00:00:00Z",
        },
        ...items(931, 939),
      ],
      oldest_ordinal: 930,
      has_more: true,
    });

    // The row is addressed by `platform_msg_id`; nothing named `m930` exists,
    // so a jump that built one would find no anchor and quietly do nothing.
    expect(rowIds()).toContain("pm-abc");
    expect(rowIds()).not.toContain("m930");
    expect(ringedRowId()).toBe("pm-abc");
  });

  it("pages backward until the ordinal is covered, then jumps", async () => {
    await open(60, 999);
    await jumpTo(900);

    // One page asked for, from the current floor.
    expect(fetches()).toEqual([940]);
    expect(ringedRowId()).toBeNull();

    await pushFrame({
      kind: "history_page",
      rows: items(890, 939),
      oldest_ordinal: 890,
      has_more: true,
    });

    // Covered now — it lands rather than asking for another page.
    expect(fetches()).toEqual([940]);
    expect(ringedRowId()).toBe("m900");
  });

  it("keeps paging across several pages before it lands", async () => {
    await open(60, 999);
    await jumpTo(850);

    await pushFrame({
      kind: "history_page",
      rows: items(890, 939),
      oldest_ordinal: 890,
      has_more: true,
    });
    expect(ringedRowId()).toBeNull();
    expect(fetches()).toEqual([940, 890]);

    await pushFrame({
      kind: "history_page",
      rows: items(840, 889),
      oldest_ordinal: 840,
      has_more: true,
    });
    expect(ringedRowId()).toBe("m850");
  });

  it("stops at the top of the thread and says so, rather than asking forever", async () => {
    await open(60, 999);
    await jumpTo(1);

    await pushFrame({
      kind: "history_page",
      rows: items(930, 939),
      oldest_ordinal: 930,
      has_more: false,
    });

    expect(fetches()).toEqual([940]);
    expect(document.querySelectorAll(".bubble.notice")).toHaveLength(1);
    expect(ringedRowId()).toBeNull();
  });

  // The infinite loop this design can actually produce: an empty page leaves the
  // floor PUT (`prependOlder`'s own rule) while `has_more` stays true, so every
  // re-entry sees the same un-covered ordinal and the same "there is more" —
  // forever. Only the budget stops it.
  it("is bounded when empty pages keep claiming there is more", async () => {
    await open(60, 999);
    await jumpTo(1);

    for (let i = 0; i < 30; i++) {
      await pushFrame({
        kind: "history_page",
        rows: [],
        oldest_ordinal: null,
        has_more: true,
      });
    }

    // JUMP_PAGE_BUDGET pages, and not one more.
    expect(fetches()).toHaveLength(12);
    expect(fetches().every((o) => o === 940)).toBe(true);
    expect(document.querySelectorAll(".bubble.notice")).toHaveLength(1);
  });

  // An ordinal INSIDE the loaded window that renders no row (a tool row, say)
  // can never be produced by paging — paging only ever loads rows further back.
  // Asking for pages anyway would drag the reader through the whole history to
  // fail at the end of it.
  it("gives up on an ordinal inside the window that renders no row", async () => {
    await open(60, 999);
    await jumpTo(935);

    // The page covers 930-939 but omits 935 — the shape a tool row leaves, since
    // the display read emits only the rows a reader can see.
    await pushFrame({
      kind: "history_page",
      rows: [...items(930, 934), ...items(936, 939)],
      oldest_ordinal: 930,
      has_more: true,
    });

    // Below the floor it would page; at or above it, paging can only ever load
    // rows FURTHER BACK, so no number of pages produces this one. Stop at one.
    expect(fetches()).toEqual([940]);
    expect(document.querySelectorAll(".bubble.notice")).toHaveLength(1);
    expect(ringedRowId()).toBeNull();
  });
});
