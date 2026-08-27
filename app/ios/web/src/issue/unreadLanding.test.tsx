import { act, fireEvent, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";

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

function payload(firstUnread?: string): IssuePayload {
  return { issue: card, events: EVENTS, runs: [], people: PEOPLE, firstUnread };
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

beforeEach(() => {
  scrolled = vi.fn();
  Element.prototype.scrollIntoView = scrolled;
});

describe("opening a card at the unread boundary", () => {
  it("draws the rule above the first unread entry and lands on it", () => {
    page();
    deliver(payload("e2"));

    const marker = rule();
    expect(marker).not.toBeNull();
    expect(marker?.textContent).toBe("New");
    expect(marker?.nextElementSibling?.textContent).toContain("the first new one");
    expect(scrolled).toHaveBeenCalledTimes(1);
    expect(scrolled.mock.instances[0]).toBe(marker);
    expect(scrolled).toHaveBeenCalledWith({ block: "start" });
  });

  // Painting stamps the card read, so the next response omits the boundary;
  // the page must keep the boundary it already showed the reader.
  it("holds the rule still once the card has been stamped read", () => {
    page();
    deliver(payload("e2"));
    deliver(payload(undefined));

    expect(rule()).not.toBeNull();
    expect(scrolled).toHaveBeenCalledTimes(1);
  });

  it("a card with nothing new opens at the top", () => {
    page();
    deliver(payload(undefined));

    expect(rule()).toBeNull();
    expect(scrolled).not.toHaveBeenCalled();
  });

  it("never scrolls a reader who has already taken the page", () => {
    const { container } = page();
    deliver(payload(undefined));
    fireEvent.pointerDown(container.querySelector(".issue-page") as Element);
    deliver(payload("e2"));

    expect(rule()).not.toBeNull();
    expect(scrolled).not.toHaveBeenCalled();
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
