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
  vi.useFakeTimers();
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  Reflect.deleteProperty(document, "scrollingElement");
});

const shown = () => postMessage.mock.calls.filter(([message]) => message.type === "shown");
const frame = () => act(() => { vi.advanceTimersToNextFrame(); });
const settle = () => { frame(); frame(); };

// A reused WKWebView can restore the outgoing scroll offset after React has
// mounted the incoming conversation. Only a session change needs the veil.
describe("the retarget veil", () => {
  it("keeps WebKit's replay of the outgoing offset behind the veil", () => {
    document.body.innerHTML = '<div id="root"></div>';
    const scroller = { scrollHeight: 1600, clientHeight: 800, scrollTop: 1200 };
    Object.defineProperty(document, "scrollingElement", { configurable: true, value: scroller });
    const html = document.documentElement;
    init("session-a");
    expect(html.classList.contains("retargeting")).toBe(false);
    init("session-b");
    expect(html.classList.contains("retargeting")).toBe(true);
    render(
      <I18nextProvider i18n={i18n}>
        <Transcript sessionId="session-b" restored={null} initialConnEpoch={0} />
      </I18nextProvider>,
    );
    expect(scroller.scrollTop).toBe(800);
    frame();
    scroller.scrollTop = 1200;
    frame();
    expect(scroller.scrollTop).toBe(800);
    expect(html).toHaveClass("retargeting");
    expect(shown()).toHaveLength(0);
    settle();
    expect(html).not.toHaveClass("retargeting");
    expect(shown()).toHaveLength(1);
    expect(shown()[0][0].sessionId).toBe("session-b");
  });

  it("does not conceal a same-session re-init", () => {
    init("same-session");
    render(<Transcript restored={null} initialConnEpoch={0} />);
    settle();
    init("same-session");
    expect(document.documentElement.classList.contains("retargeting")).toBe(false);
  });
});

describe("first layout", () => {
  it("waits past the retarget failsafe for geometry, then pins before notifying native", () => {
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
    settle();
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(1);
  });

  it("pins as the keyboard spacer resizes and stops writing when it settles", () => {
    const observers = new Map<Element, ResizeObserverCallback>();
    vi.spyOn(window, "ResizeObserver").mockImplementation(function (callback): ResizeObserver {
      return {
        observe(element) { observers.set(element, callback); },
        unobserve(element) { observers.delete(element); },
        disconnect() {
          for (const [element, listener] of observers) {
            if (listener === callback) observers.delete(element);
          }
        },
      };
    });
    let top = 0;
    const write = vi.fn((y: number) => { top = y; });
    const scroller = {
      clientHeight: 678, scrollHeight: 711,
      get scrollTop() { return top; },
      set scrollTop(value) { write(value); },
    };
    Object.defineProperty(document, "scrollingElement", { configurable: true, value: scroller });
    init("typing", 96);
    const { container, unmount } = render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={96} />);
    settle();
    const log = container.querySelector(".chat-log");
    const space = container.querySelector(".composer-space")!;
    expect(space.parentElement?.previousElementSibling).toBe(log);
    const resize = observers.get(space)!;
    const notify = () => resize([], {} as ResizeObserver);
    for (const inset of [380, 396, 96, 380, 96]) {
      act(() => {
        window.baybo.setBottomInset(inset, "typing");
        scroller.scrollHeight = 615 + inset;
        notify();
      });
      expect(scroller.scrollTop).toBe(scroller.scrollHeight - scroller.clientHeight);
      expect(container.querySelector(".chat-log")).toBe(log);
      expect(document.documentElement).not.toHaveClass("awaiting-inset");
      expect(document.documentElement).not.toHaveClass("retargeting");
      expect(document.documentElement.style.getPropertyValue("--thread-bottom-inset")).toBe(`${inset}px`);
      write.mockClear();
      act(() => { notify(); vi.advanceTimersByTime(400); });
      expect(write).not.toHaveBeenCalled();
    }
    expect(shown()).toHaveLength(1);
    // A tap in the transcript resigns the native field. The next focus can
    // deliver an off-edge scroll before the spacer observer gets its turn.
    act(() => {
      window.dispatchEvent(new TouchEvent("touchstart"));
      window.dispatchEvent(new Event("touchend"));
      window.baybo.setBottomInset(396, "typing");
      scroller.scrollHeight = 1011;
      window.dispatchEvent(new Event("scroll"));
      notify();
    });
    expect(top).toBe(333);
    act(() => {
      window.baybo.setBottomInset(96, "typing");
      scroller.scrollHeight = 711;
      notify();
    });
    write.mockClear();
    act(() => {
      window.dispatchEvent(new TouchEvent("touchstart"));
      window.baybo.setBottomInset(380, "typing");
      scroller.scrollHeight = 995;
      notify();
    });
    expect(write).not.toHaveBeenCalled();
    act(() => {
      top = 0;
      window.dispatchEvent(new Event("scroll"));
      window.dispatchEvent(new Event("touchend"));
      notify();
    });
    expect(write).not.toHaveBeenCalled();
    unmount();
    expect(observers.has(space)).toBe(false);
  });

  it("consumes geometry buffered before the incoming tree subscribes", () => {
    init("buffered-geometry", null);
    window.baybo.setBottomInset(120, "buffered-geometry");
    render(<Transcript restored={null} initialConnEpoch={0} initialBottomInset={null} />);
    settle();
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(1);
  });

  it("waits again for a new session, but does not conceal a displayed same-session re-init", () => {
    init("shown-a", 100);
    const tree = render(
      <Transcript key="shown-a" restored={null} initialConnEpoch={0} initialBottomInset={100} />,
    );
    settle();
    init("shown-a", null);
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);

    init("waiting-b", null);
    tree.rerender(
      <Transcript key="waiting-b" restored={null} initialConnEpoch={0} initialBottomInset={null} />,
    );
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(true);
    expect(shown()).toHaveLength(1);
    act(() => { window.baybo.setBottomInset(90, "waiting-b"); });
    settle();
    expect(document.documentElement.classList.contains("awaiting-inset")).toBe(false);
    expect(shown()).toHaveLength(2);
  });
});
