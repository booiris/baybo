import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "../i18n";
import { forgetAvatars } from "./avatars";
import type { IssuePayload } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { Actor, IssueDetail, IssueEvent } from "./types";

/// A card reads as a thread: what somebody said is a boxed post with their
/// face beside it, and what the board did is one line.
///
/// The rules worth pinning are the ones that fail SILENTLY — a face that says
/// nothing, a name that says the wrong thing, and a page that asks native for
/// the same picture once per row that draws it.

const blobRequests: string[] = [];

vi.mock("../bridge", async (importOriginal) => {
  const real = await importOriginal<typeof import("../bridge")>();
  return {
    ...real,
    blobObjectUrl: (blobId: string) => {
      blobRequests.push(blobId);
      return Promise.resolve(`blob:${blobId}`);
    },
  };
});

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
  unread: 0,
  last_run_failed: false,
  approval_pending: false,
  opened_by_agent: false,
  created_at_ms: 1,
  updated_at_ms: 9,
};

const AGENT: Actor = { kind: "agent", id: "a-dev", handle: "dev-1" };

function comment(id: string, text: string, actor: Actor = AGENT): IssueEvent {
  return { id, number: 7, actor, body: { kind: "comment", text }, created_at_ms: 1 };
}

function moved(id: string): IssueEvent {
  return {
    id,
    number: 7,
    actor: { kind: "system" },
    body: { kind: "moved", from: "todo", to: "in_progress" },
    created_at_ms: 1,
  };
}

function opened(id: string): IssueEvent {
  return { id, number: 7, actor: { kind: "user" }, body: { kind: "opened" }, created_at_ms: 1 };
}

function deliver(events: IssueEvent[], avatar?: string): void {
  const payload: IssuePayload = {
    issue: card,
    events,
    runs: [],
    people: { "a-dev": { handle: "dev-1", monogram: "D1", avatar } },
  };
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  act(() => {
    page.deliver(payload);
  });
}

function mount() {
  render(
    <I18nextProvider i18n={i18n}>
      <IssuePage />
    </I18nextProvider>,
  );
}

/// The Activity's heads only — the description is a post too, and its box is
/// titled by what it IS ("Description") on a card whose opening nothing
/// recorded.
function heads(): string[] {
  return [...document.querySelectorAll(".issue-activity .issue-box-who")].map(
    (el) => el.textContent,
  );
}

beforeEach(() => {
  blobRequests.length = 0;
  forgetAvatars();
});

describe("a comment is a post", () => {
  it("boxes it under a head naming its author, with their monogram for a face", () => {
    mount();
    deliver([comment("e1", "pulled the flag")]);

    const box = document.querySelector(".issue-entry.comment .issue-box");
    expect(box).not.toBeNull();
    expect(box?.querySelector(".issue-box-body")?.textContent).toContain("pulled the flag");
    expect(heads()).toContain("@dev-1");
    // The monogram is NATIVE's — it is unique across the team, which one
    // handle on its own cannot tell you.
    expect(document.querySelector(".issue-activity .issue-face")?.textContent).toBe("D1");
  });

  /// This said "board" until 2026-08-25: `actorHandle` answers `null` for both
  /// a user and the system, and the row printed the system's word for either.
  it("calls the operator themselves, not the board", () => {
    mount();
    deliver([comment("e1", "mine", { kind: "user" }), comment("e2", "theirs")]);

    expect(heads()).toEqual(["You", "@dev-1"]);
    expect(document.querySelector(".issue-activity .issue-face.user")).not.toBeNull();
  });

  it("draws the picture when there is one, and asks for it once for the whole card", async () => {
    mount();
    deliver([comment("e1", "one"), comment("e2", "two"), comment("e3", "three")], "blob-7");
    await act(async () => {
      await Promise.resolve();
    });

    expect(blobRequests).toEqual(["blob-7"]);
    const faces = [...document.querySelectorAll(".issue-activity img.issue-face")];
    expect(faces).toHaveLength(3);
    expect(faces.every((el) => el.getAttribute("src") === "blob:blob-7")).toBe(true);
  });

  /// Machinery has no body worth a box: a `moved` is one sentence, and giving
  /// it the same frame as a paragraph makes the card a wall of rectangles.
  it("leaves the board's own entries as a line", () => {
    mount();
    deliver([moved("e1"), comment("e2", "said")]);
    // Machinery is folded shut, so open the run to see the line it holds.
    act(() => {
      document.querySelector<HTMLButtonElement>(".issue-fold button")?.click();
    });

    expect(document.querySelectorAll(".issue-entry.comment")).toHaveLength(1);
    const line = document.querySelector(".issue-line");
    expect(line?.textContent).toContain("moved it");
    expect(line?.querySelector(".issue-line-dot")).not.toBeNull();
    expect(line?.querySelector(".issue-box")).toBeNull();
  });

  /// The description's head already says who opened the card. Leaving the
  /// `opened` entry in the Activity as well prints the same fact twice, a
  /// screen apart — so it is hoisted, not repeated.
  it("hoists the opening into the description and drops it from the timeline", () => {
    mount();
    deliver([opened("e1"), comment("e2", "said")]);

    const head = document.querySelector(".issue-box-head");
    expect(head?.textContent).toContain("You");
    expect(head?.textContent).toContain("opened this card");
    expect(document.querySelector(".issue-activity")?.textContent).not.toContain("opened this");
  });

  /// A card nobody's timeline records the opening of still has a box, titled
  /// by what it IS — the alternative was a head reading "board", which is
  /// what the page did before it had heads at all.
  it("titles the description by its name when there is no opener", () => {
    mount();
    deliver([comment("e2", "said")]);

    expect(document.querySelector(".issue-box-head")?.textContent).toContain("Description");
  });
});
