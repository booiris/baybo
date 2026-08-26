import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { describe, expect, it } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { IssueDetail, IssueEvent, IssueRun } from "./types";

/// What the top of a card says, and in what order.
///
/// A card is opened to find out two things — what it is called and what it
/// says — and the pickers used to sit between them, so the first screen was a
/// title, a row of pills, a line of provenance and then, if there was room,
/// the first sentence. State is read second: it is now one band UNDER the
/// description, and this file is what stops it drifting back up.
///
/// The other half is the hue. It rides on the VALUE (`data-status` /
/// `data-priority`) rather than on a class per chip, so the chips and the
/// sub-issue dots read one table — asserting the attribute is asserting that
/// the table can reach the element at all.

const card: IssueDetail = {
  number: 12,
  project_id: "01JBOARD",
  title: "the dial loop drops its subscription",
  description: "it stops resubscribing after the second redial",
  status: "in_progress",
  priority: "high",
  assignee: "a-dev",
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

const OPENED: IssueEvent = {
  id: "e1",
  number: 12,
  actor: { kind: "user" },
  body: { kind: "opened" },
  created_at_ms: 1,
};

function liveRun(): IssueRun {
  return {
    number: 12,
    attempt: 3,
    agent_id: "a-dev",
    status: "running",
    trigger: "started",
    session_id: "s-3",
    created_at_ms: 2,
    started_at_ms: 3,
  };
}

function settledRun(): IssueRun {
  return {
    number: 12,
    attempt: 2,
    agent_id: "a-dev",
    status: "failed",
    trigger: "retry",
    session_id: "s-2",
    error: "the workdir is held by card #17",
    created_at_ms: 1,
    settled_at_ms: 2,
  };
}

function mount(payload: Partial<IssuePayload> = {}): void {
  render(
    <I18nextProvider i18n={i18n}>
      <IssuePage />
    </I18nextProvider>,
  );
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  act(() => {
    page.deliver({
      issue: card,
      events: [OPENED],
      runs: [],
      people: { "a-dev": { handle: "dev-1", monogram: "D1" } },
      ...payload,
    });
  });
}

/// Where a node sits in the page, as a plain index — `compareDocumentPosition`
/// says "before" but not "before what else", and the order of three things is
/// what this file is about.
function order(): string[] {
  const page = document.querySelector(".issue-page");
  return [...(page?.children ?? [])].map((el) => el.className);
}

describe("the head, then the text, then the state", () => {
  it("puts the description above the chips, and the chips in their own band", () => {
    mount();

    const positions = order();
    const head = positions.findIndex((c) => c.includes("issue-head"));
    const body = positions.findIndex((c) => c.includes("issue-body"));
    const state = positions.findIndex((c) => c.includes("issue-state"));

    expect(head).toBeGreaterThanOrEqual(0);
    expect(body).toBeGreaterThan(head);
    expect(state).toBeGreaterThan(body);
  });

  /// The head is the title and the provenance line, and NOTHING you can press
  /// to change the card: a chip left behind in the header is the whole bug
  /// this order was changed to fix.
  it("leaves no control in the head", () => {
    mount();

    expect(document.querySelector(".issue-head .issue-chip")).toBeNull();
    expect(document.querySelector(".issue-state .issue-chip")).not.toBeNull();
    expect(document.querySelector(".issue-head")?.textContent).toContain("opened by You");
  });

  /// The hue is keyed by the wire value. A chip that renders the right word
  /// with no attribute is a chip the colour table cannot see — and it looks
  /// exactly like a chip whose value has no hue.
  it("keys the chips by the value the hue table reads", () => {
    mount();

    const status = document.querySelector(".issue-chip[data-status]");
    const priority = document.querySelector(".issue-chip[data-priority]");
    expect(status?.getAttribute("data-status")).toBe("in_progress");
    expect(priority?.getAttribute("data-priority")).toBe("high");
    // The assignee is a person, not a state: no hue, so no key.
    const chips = [...document.querySelectorAll(".issue-chip")];
    expect(chips).toHaveLength(3);
    const assignee = chips[2];
    expect(assignee.hasAttribute("data-status")).toBe(false);
    expect(assignee.hasAttribute("data-priority")).toBe(false);
  });

  it("keys a sub-issue's dot off the same table", () => {
    mount({
      issue: { ...card, sub_issues: { done: 1, total: 2 } },
      children: [
        { number: 13, title: "the first", status: "done" },
        { number: 14, title: "the second", status: "todo" },
      ],
    });

    const dots = [...document.querySelectorAll(".issue-sub-dot")].map((d) =>
      d.getAttribute("data-status"),
    );
    expect(dots).toEqual(["done", "todo"]);
  });
});

describe("the runs live in the native ⋯", () => {
  /// The card page lists no attempts at all any more. A settled run is
  /// history — read once, when something went wrong — and it sat between the
  /// card's state and its comments on every single open.
  it("draws no run list, however many attempts the card has", () => {
    mount({ runs: [liveRun(), settledRun()] });

    expect(document.querySelector(".issue-runs")).toBeNull();
    expect(document.body.textContent).not.toContain("#2");
  });

  /// The one exception, and it is not a list: the run holding the card right
  /// now is its state, so it reads as a line in the state band with the way
  /// into its transcript on the end.
  it("keeps the live run as one line in the state band", () => {
    mount({ runs: [liveRun(), settledRun()] });

    const row = document.querySelector(".issue-state .issue-run");
    expect(row).not.toBeNull();
    expect(row?.getAttribute("data-run")).toBe("running");
    expect(row?.textContent).toContain("@dev-1");
    expect(row?.querySelector(".issue-run-open")).not.toBeNull();
  });

  it("says nothing about runs when none is holding the card", () => {
    mount({ runs: [settledRun()] });

    expect(document.querySelector(".issue-run")).toBeNull();
  });
});
