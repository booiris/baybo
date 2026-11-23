import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

vi.stubGlobal(
  "IntersectionObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

import { Transcript } from "./Transcript";
import i18n from "./i18n";

const { postMessage } = vi.hoisted(() => {
  const postMessage = vi.fn();
  window.webkit = { messageHandlers: { baybo: { postMessage } } };
  return { postMessage };
});

const init = (sessionId: string, bottomInset: number | null = 0) =>
  window.baybo.init({
    language: "en",
    sessionId,
    restoredState: null,
    connEpoch: 0,
    expandUnansweredTail: false,
    bottomInset,
  });

beforeEach(() => {
  postMessage.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
  Reflect.deleteProperty(document, "scrollingElement");
});

const shown = () => postMessage.mock.calls.filter(([message]) => message.type === "shown");

// The cross-session retarget veil (bridge.ts concealForRetarget): the moment a
// DIFFERENT session's init lands, the old conversation is hidden by a
// synchronous DOM write; the incoming tree lifts the veil from its mount
// layout effect, in the same frame its own content paints. A same-session
// re-init must NOT conceal — nothing remounts, so nothing would ever reveal
// (the failsafe would, but only after a visible blank).
describe("the retarget veil", () => {
  it("conceals on a cross-session init and reveals on the incoming mount", () => {
    document.body.innerHTML = '<div id="root"></div>';
    const html = document.documentElement;
    init("session-a");
    expect(html.classList.contains("retargeting")).toBe(false);
    init("session-b");
    expect(html.classList.contains("retargeting")).toBe(true);
    render(
      <I18nextProvider i18n={i18n}>
        <Transcript restored={null} initialConnEpoch={0} />
      </I18nextProvider>,
    );
    expect(html.classList.contains("retargeting")).toBe(false);
  });

  it("does not conceal a same-session re-init", () => {
    init("session-b");
    expect(document.documentElement.classList.contains("retargeting")).toBe(false);
  });
});

describe("first layout", () => {
  it("waits past the retarget failsafe for geometry, then pins before notifying native", () => {
    vi.useFakeTimers();
    let scrollTop = 0;
    let scrollAtReveal = -1;
    const scroller = {
      get scrollHeight() {
        const inset = document.documentElement.style.getPropertyValue("--thread-bottom-inset");
        return 1200 + Number.parseInt(inset);
      },
      get scrollTop() {
        return scrollTop;
      },
      set scrollTop(value: number) {
        scrollTop = Math.min(value, this.scrollHeight - 800);
      },
      clientHeight: 800,
    };
    Object.defineProperty(document, "scrollingElement", { configurable: true, value: scroller });
    postMessage.mockImplementation((message) => {
      if (message.type === "shown") scrollAtReveal = scrollTop;
    });
    document.documentElement.style.setProperty("--thread-bottom-inset", "0px");
    init("late-geometry", null);
    render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={null} />);

    act(() => { vi.advanceTimersByTime(450); });
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(true);
    expect(shown()).toHaveLength(0);

    act(() => { window.baybo.setBottomInset(300, "outgoing-session"); });
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(true);
    expect(shown()).toHaveLength(0);

    act(() => { window.baybo.setBottomInset(120, "late-geometry"); });
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(document.documentElement.classList.contains("retargeting")).toBe(false);
    expect(shown()).toHaveLength(1);
    expect(scrollAtReveal).toBe(520);
  });

  it("reveals a zero-inset read-only page without waiting for a composer event", () => {
    init("read-only", 0);
    render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={0} />);
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(1);
  });

  it("keeps the visible conversation on screen through repeated keyboard changes", () => {
    vi.useFakeTimers();
    const scroller = { clientHeight: 678, scrollHeight: 711, scrollTop: 0 };
    Object.defineProperty(document, "scrollingElement", { configurable: true, value: scroller });
    init("typing", 96);
    const { container } = render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={96} />);
    const log = container.querySelector(".chat-log");
    expect(log).toBeInTheDocument();
    for (const inset of [96, 380, 396, 96, 380, 96]) {
      act(() => {
        scroller.scrollHeight = scroller.clientHeight + inset - 96;
        window.baybo.setBottomInset(inset, "typing");
        vi.advanceTimersByTime(20);
      });
      expect(scroller.scrollTop).toBe(scroller.scrollHeight - scroller.clientHeight);
      expect(container.querySelector(".chat-log")).toBe(log);
      expect(document.documentElement).not.toHaveClass("awaiting-inset");
      expect(document.documentElement).not.toHaveClass("retargeting");
      expect(document.documentElement.style.getPropertyValue("--thread-bottom-inset")).toBe(`${inset}px`);
    }
    expect(shown()).toHaveLength(1);
  });

  it("consumes geometry buffered before the incoming tree subscribes", () => {
    init("buffered-geometry", null);
    window.baybo.setBottomInset(120, "buffered-geometry");
    render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={null} />);
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(1);
  });

  it("waits again for a new session, but does not conceal a displayed same-session re-init", () => {
    init("shown-a", 100);
    const tree = render(
      <Transcript key="shown-a" restored={null} initialConnEpoch={0} initialBottomInset={100} />,
    );
    init("shown-a", null);
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);

    init("waiting-b", null);
    tree.rerender(
      <Transcript key="waiting-b" restored={null} initialConnEpoch={0} initialBottomInset={null} />,
    );
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(true);
    expect(shown()).toHaveLength(1);
    act(() => { window.baybo.setBottomInset(90, "waiting-b"); });
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(2);
  });
});
