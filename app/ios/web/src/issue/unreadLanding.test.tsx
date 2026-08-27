import { act, fireEvent, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { IssueDetail, IssueEvent } from "./types";

/// The card's Activity list: where it opens, and what it keeps folded away.
///
/// Opening a card where the reading stopped.
///
/// The boundary is the gateway's (`IssueTimelineDto.first_unread`) and this
/// page only decides what to do with it, so what is worth pinning here is the
/// *what to do*: draw the rule above the right row, land there once, and hold
/// the rule still afterwards. That last one is the whole reason the state is
/// frozen — painting the card stamps it read, and the refetch a second later
/// answers with no boundary at all.
///
/// jsdom has no layout, so the landing itself is asserted through
/// `scrollIntoView` (stubbed) rather than a scroll offset. What that cannot
/// say anything about is whether the rule clears the floating native header —
/// that is `scroll-margin-top` in issue.css, and only a device can confirm it.

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
  // Set at import time by `./bridge`, which `IssuePage` pulls in — the global
  // is optional for the transcript entry, which never defines it.
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

  /// Painting the card stamps it read, which invalidates the timeline, which
  /// refetches — and that answer carries no boundary. A rule that tracked the
  /// payload would vanish under the reader a second after they arrived.
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

  /// The mirror paints first and the live answer lands a moment later. If the
  /// reader has started reading in that gap the page is theirs — the rule
  /// still marks the boundary, but nothing moves under their finger.
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
  /// **A run of one is drawn, not folded** (2026-08-26). It was uniform for a
  /// while — every run collapsed, a lone `moved` included, so the comments
  /// would be findable down a column of identical closed lines. What that
  /// bought in practice was a control hiding exactly one line and spending one
  /// to say so, on most of the runs a card has.
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

  /// The exception, and it has to be one: the run carrying the boundary is
  /// what the page just scrolled to, and landing a reader on a closed line is
  /// landing them on nothing.
  it("opens the run the card lands on", () => {
    page();
    // The boundary arrives on the SECOND delivery, as it does in the app —
    // the mirror paints first and carries none. A row seeded at mount would
    // read the landing before it exists.
    deliver(withEvents([comment("e1", "before"), moved("e5"), moved("e6")]));
    expect(fold()?.getAttribute("aria-expanded")).toBe("false");

    deliver(withEvents([comment("e1", "before"), moved("e5"), moved("e6")], "e5"));
    expect(fold()?.getAttribute("aria-expanded")).toBe("true");
    expect(document.body.textContent).toContain("moved it");
    expect(rule()?.nextElementSibling?.textContent).toContain("2 events");
  });
});
