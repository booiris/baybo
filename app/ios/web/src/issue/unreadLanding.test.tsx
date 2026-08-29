import { act, fireEvent, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { IssueDetail, IssueEvent } from "./types";


const card: IssueDetail = {
  number: 7,
  project_id: "01JBOARD",
  title: "the long thread",
  description: "",
  status: "in_progress",
  priority: "none",
  position: 0,
  pinned: false,
  stage: 0,
  unread: 2,
  last_run_failed: false,
  approval_pending: false,
  opened_by_agent: false,
  created_at_ms: 1,
  updated_at_ms: 9,
};

function comment(id: string, text: string): IssueEvent {
  return {
    id,
    number: 7,
    actor: { kind: "agent", id: "a-dev", handle: "dev-1" },
    body: { kind: "comment", text },
    created_at_ms: Number(id.slice(1)),
  };
}

function moved(id: string): IssueEvent {
  return {
    id,
    number: 7,
    actor: { kind: "system" },
    body: { kind: "moved", from: "in_progress", to: "review" },
    created_at_ms: Number(id.slice(1)),
  };
}

function withEvents(events: IssueEvent[], firstUnread?: string): IssuePayload {
  return { issue: card, events, runs: [], people: PEOPLE, firstUnread };
}

function fold(): HTMLButtonElement | null {
  return document.querySelector(".issue-fold button");
}

const PEOPLE = { "a-dev": { handle: "dev-1", monogram: "D1" } };

const EVENTS: IssueEvent[] = [
  comment("e1", "read this one"),
  comment("e2", "the first new one"),
  comment("e3", "and a later one"),
];

function payload(firstUnread?: string, timelineLive = false): IssuePayload {
  return { issue: card, events: EVENTS, runs: [], people: PEOPLE, firstUnread, timelineLive };
}

function deliver(p: IssuePayload): void {
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  act(() => {
    page.deliver(p);
  });
}

function page() {
  return render(
    <I18nextProvider i18n={i18n}>
      <IssuePage />
    </I18nextProvider>,
  );
}

function rule(): HTMLElement | null {
  return document.querySelector("[data-unread-rule]");
}

let scrolled: ReturnType<typeof vi.fn>;
const originalScrollHeight = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollHeight",
);
const originalClientHeight = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "clientHeight",
);
const originalScrollTo = HTMLElement.prototype.scrollTo;

beforeEach(() => {
  scrolled = vi.fn();
  HTMLElement.prototype.scrollTo = scrolled;
  Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
    configurable: true,
    get: () => 900,
  });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", {
    configurable: true,
    get: () => 300,
  });
});

afterEach(() => {
  if (originalScrollHeight === undefined) {
    Reflect.deleteProperty(HTMLElement.prototype, "scrollHeight");
  } else {
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", originalScrollHeight);
  }
  if (originalClientHeight === undefined) {
    Reflect.deleteProperty(HTMLElement.prototype, "clientHeight");
  } else {
    Object.defineProperty(HTMLElement.prototype, "clientHeight", originalClientHeight);
  }
  HTMLElement.prototype.scrollTo = originalScrollTo;
});

function scroller(): HTMLElement | null {
  return document.querySelector<HTMLElement>(".issue-page");
}

describe("opening a card at its latest activity", () => {
  it("keeps a loading animation at the bottom until the live timeline arrives", () => {
    page();

    const coldLoader = document.querySelector(".issue-loading .issue-loading-row");
    expect(coldLoader).toHaveAttribute("role", "status");
    expect(coldLoader?.textContent).toBe("Loading card…");

    deliver(payload(undefined));
    const tailLoader = document.querySelector(".issue-tail-loading");
    expect(tailLoader).toHaveAttribute("role", "status");
    expect(tailLoader?.textContent).toBe("Loading latest activity…");
    expect(scroller()?.lastElementChild).toBe(tailLoader);

    deliver(payload(undefined, true));
    expect(document.querySelector(".issue-tail-loading")).toBeNull();
  });

  it("draws the unread rule but lands at the bottom", () => {
    page();
    deliver(payload("e2"));

    const marker = rule();
    expect(marker).not.toBeNull();
    expect(marker?.textContent).toBe("New");
    expect(marker?.nextElementSibling?.textContent).toContain("the first new one");
    expect(scroller()?.scrollTop).toBe(900);
  });

  it("holds the unread rule still once the card has been stamped read", () => {
    page();
    deliver(payload("e2"));
    deliver(payload(undefined));

    expect(rule()).not.toBeNull();
    expect(scroller()?.scrollTop).toBe(900);
  });

  it("a card with nothing unread still opens at the bottom", () => {
    page();
    deliver(payload(undefined));

    expect(rule()).toBeNull();
    expect(scroller()?.scrollTop).toBe(900);
  });

  it("follows a mirror to the bottom again when the live timeline lands", () => {
    page();
    deliver(payload(undefined));
    const el = scroller();
    if (el === null) throw new Error("issue scroller missing");
    el.scrollTop = 500;

    deliver(
      {
        ...payload(undefined, true),
        events: [...EVENTS, comment("e8", "the newest live comment")],
      },
    );

    expect(el.scrollTop).toBe(900);
  });

  it("waits for the dock inset before releasing the initial bottom", () => {
    page();
    deliver(payload(undefined, true));
    const el = scroller();
    if (el === null) throw new Error("issue scroller missing");
    el.scrollTop = 400;

    act(() => window.issuePage?.setBottomInset(96));
    expect(el.scrollTop).toBe(900);

    el.scrollTop = 240;
    act(() => window.issuePage?.setBottomInset(104));
    expect(el.scrollTop).toBe(240);
  });

  it("never scrolls a reader who has already taken the page", () => {
    const { container } = page();
    deliver(payload(undefined));
    const el = scroller();
    if (el === null) throw new Error("issue scroller missing");
    fireEvent.pointerDown(container.querySelector(".issue-page") as Element);
    el.scrollTop = 120;
    deliver(payload("e2", true));

    expect(rule()).not.toBeNull();
    expect(el.scrollTop).toBe(120);
  });

  it("obeys the native header's smooth scroll-to-top command", () => {
    page();
    deliver(payload(undefined));

    window.issuePage?.scrollToTop();

    expect(scrolled).toHaveBeenCalledWith({ top: 0, behavior: "smooth" });
  });
});

describe("machinery folds away", () => {
  it("draws a run of one rather than folding it", () => {
    page();
    deliver(withEvents([comment("e1", "before"), moved("e5"), comment("e9", "after")]));

    expect(fold()).toBeNull();
    expect(document.body.textContent).toContain("moved it");
    expect(document.body.textContent).toContain("before");
  });

  it("opens on a press and closes again on the next one", () => {
    page();
    deliver(withEvents([moved("e5"), moved("e6")]));
    const button = fold();

    expect(button?.getAttribute("aria-expanded")).toBe("false");
    act(() => button?.click());
    expect(fold()?.getAttribute("aria-expanded")).toBe("true");
    expect(document.body.textContent).toContain("moved it");
    act(() => fold()?.click());
    expect(fold()?.getAttribute("aria-expanded")).toBe("false");
    expect(document.body.textContent).not.toContain("moved it");
  });

  it("restores the scroll and fold state when a warm slot returns to this card", () => {
    render(
      <I18nextProvider i18n={i18n}>
        <IssuePage
          targetId="visit-restored"
          initialState={{ scrollTop: 143, folds: { "sys-e5": true } }}
        />
      </I18nextProvider>,
    );
    deliver(withEvents([moved("e5"), moved("e6")]));

    const scroller = document.querySelector<HTMLElement>(".issue-page");
    expect(scroller?.scrollTop).toBe(143);
    expect(fold()?.getAttribute("aria-expanded")).toBe("true");
    expect(window.issuePage?.snapshotState()).toEqual({
      scrollTop: 143,
      folds: { "sys-e5": true },
    });
  });

  it("opens the run the card lands on", () => {
    page();
    deliver(withEvents([comment("e1", "before"), moved("e5"), moved("e6")]));
    expect(fold()?.getAttribute("aria-expanded")).toBe("false");

    deliver(withEvents([comment("e1", "before"), moved("e5"), moved("e6")], "e5"));
    expect(fold()?.getAttribute("aria-expanded")).toBe("true");
    expect(document.body.textContent).toContain("moved it");
    expect(rule()?.nextElementSibling?.textContent).toContain("2 events");
  });
});
