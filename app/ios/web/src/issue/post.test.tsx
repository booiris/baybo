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
const nativePosts: Record<string, unknown>[] = [];

vi.mock("../bridge", async (importOriginal) => {
  const real = await importOriginal<typeof import("../bridge")>();
  return {
    ...real,
    postToNative: (message: Record<string, unknown>) => nativePosts.push(message),
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
  branch: "issue-7",
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

function runSettled(id: string, attempt: number): IssueEvent {
  return {
    id,
    number: 7,
    actor: AGENT,
    body: { kind: "run_settled", attempt, status: "failed" },
    created_at_ms: 1,
  };
}

function opened(id: string): IssueEvent {
  return { id, number: 7, actor: { kind: "user" }, body: { kind: "opened" }, created_at_ms: 1 };
}

function deliver(events: IssueEvent[], avatar?: string, pendingComments?: IssueEvent[]): void {
  const payload: IssuePayload = {
    issue: card,
    events,
    pendingComments,
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
  return [...document.querySelectorAll(".issue-activity .issue-said-who")].map(
    (el) => el.textContent,
  );
}

beforeEach(() => {
  blobRequests.length = 0;
  nativePosts.length = 0;
  forgetAvatars();
});

describe("a comment is a post", () => {
  /// The head names the author and stands OUTSIDE the box: the box holds the
  /// words and nothing else, which is what stops a comment being three nested
  /// rectangles deep.
  it("boxes the words under a bare head naming its author", () => {
    mount();
    deliver([comment("e1", "pulled the flag")]);

    const box = document.querySelector(".issue-entry.comment .issue-box");
    expect(box).not.toBeNull();
    expect(box?.textContent).toContain("pulled the flag");
    expect(box?.querySelector(".issue-said-who")).toBeNull();
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

  it("shows a delayed sending indicator and replaces the optimistic row by client id", () => {
    mount();
    const pending: IssueEvent = {
      ...comment("pending-c1", "on its way", { kind: "user" }),
      client_msg_id: "c1",
      send_state: "sending",
    };
    deliver([], undefined, [pending]);

    expect(document.querySelectorAll(".issue-entry.comment")).toHaveLength(1);
    expect(document.querySelector(".send-spinner")).not.toBeNull();

    deliver(
      [
        {
          ...comment("e1", "on its way", { kind: "user" }),
          client_msg_id: "c1",
        },
      ],
      undefined,
      [pending],
    );

    expect(document.querySelectorAll(".issue-entry.comment")).toHaveLength(1);
    expect(document.querySelector(".send-spinner")).toBeNull();
  });

  it("retries a failed optimistic row with the same client id", () => {
    mount();
    deliver([], undefined, [
      {
        ...comment("pending-c2", "try me again", { kind: "user" }),
        client_msg_id: "c2",
        send_state: "failed",
      },
    ]);

    const retry = document.querySelector<HTMLButtonElement>(".send-failed");
    expect(retry).not.toBeNull();
    act(() => retry?.click());
    expect(nativePosts).toContainEqual({
      type: "retryComment",
      targetId: "test",
      clientMsgId: "c2",
    });
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

    expect(document.querySelectorAll(".issue-entry.comment")).toHaveLength(1);
    const line = document.querySelector(".issue-line");
    expect(line?.textContent).toContain("moved it");
    expect(line?.querySelector(".issue-line-dot")).not.toBeNull();
    expect(line?.querySelector(".issue-box")).toBeNull();
  });

  it("gives a turn its own label instead of making it look like an issue number", () => {
    mount();
    deliver([runSettled("e1", 3), comment("e2", "said")]);

    expect(document.querySelector(".issue-line")?.textContent).toContain("turn 3 failed");
    expect(document.querySelector(".issue-line")?.textContent).not.toContain("#3");
  });

  /// **A run of one is not a run.** A `1 event ›` is a control that hides
  /// exactly one line and spends one saying so; two or more still collapse.
  it("draws a lone machinery entry rather than folding it", () => {
    mount();
    deliver([moved("e1"), comment("e2", "said")]);

    expect(document.querySelector(".issue-fold")).toBeNull();
    expect(document.querySelector(".issue-line")?.textContent).toContain("moved it");
  });

  it("still folds two in a row, and opens them on a press", () => {
    mount();
    deliver([moved("e1"), moved("e2"), comment("e3", "said")]);

    const fold = document.querySelector<HTMLButtonElement>(".issue-fold button");
    expect(fold?.textContent).toContain("2");
    expect(document.querySelectorAll(".issue-line")).toHaveLength(0);

    act(() => {
      fold?.click();
    });
    expect(document.querySelectorAll(".issue-line")).toHaveLength(2);
  });

  /// Who opened the card is one line of provenance under the head, not a
  /// bordered bar with a face beside it — and it is said ONCE: leaving the
  /// `opened` entry in the Activity too printed the same fact twice, a screen
  /// apart.
  it("puts the opening on the meta line and drops it from the timeline", () => {
    mount();
    deliver([opened("e1"), comment("e2", "said")]);

    expect(document.querySelector(".issue-meta")?.textContent).toContain("opened by You");
    expect(document.querySelector(".issue-activity")?.textContent).not.toContain("opened this");
  });

  /// **The description is not a post.** It is what the title is about, one
  /// line above it — so it carries no box, no author bar and no face, and
  /// every framed thing on the page belongs to somebody who wrote it.
  it("leaves the card's own text unboxed", () => {
    mount();
    deliver([opened("e1"), comment("e2", "said")]);

    expect(document.querySelectorAll(".issue-box")).toHaveLength(1);
    expect(document.querySelectorAll(".issue-activity .issue-box")).toHaveLength(1);
    expect(document.querySelector(".issue-body")).not.toBeNull();
  });

  /// Chips are for CONTROLS. The branch opens nothing and the row was five
  /// identical pills — four objects of equal weight saying unrelated things.
  it("keeps the chip row to the three things you can change", () => {
    mount();
    deliver([comment("e2", "said")]);

    const chips = [...document.querySelectorAll(".issue-chip")].map((c) => c.textContent);
    expect(chips).toHaveLength(3);
    expect(chips.join(" ")).not.toContain("⑂");
  });
});
