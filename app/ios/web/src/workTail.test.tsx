import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { Transcript } from "./Transcript";
import type { PersistedState, Row } from "./types";

/// The thread's TAIL, driven through the mounted component — because every bug
/// this pins lives in the difference between two tails that a pure reducer suite
/// reads as one.
///
/// A live frame asks "what is the last row that isn't a trailing notice", and
/// must STOP at an answer: past it lies a settled turn's card, and a bundle that
/// reaches it rewrites persisted steps, relights a finished card, and strands the
/// composer on a stop button nothing can clear (`closeWork` only ever freezes the
/// literal tail). A durable PLACEMENT asks the wider question — where the
/// trailing answer/notice run begins — because a re-delivered block has to be
/// weighed against its own turn's answer.
///
/// Both halves were once the same helper. The suite was green.
///
/// No fake layout (see transcriptScroll.test.tsx for why one exists): nothing
/// below reads geometry, only which cards are in the tree and what state they
/// are in.

vi.stubGlobal(
  "IntersectionObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

const TURN_START = "2026-08-16T12:00:00.000Z";

function mount(restored: PersistedState | null, expandUnansweredTail = false): void {
  render(
    <I18nextProvider i18n={i18n}>
      <Transcript
        restored={restored}
        initialConnEpoch={0}
        expandUnansweredTail={expandUnansweredTail}
      />
    </I18nextProvider>,
  );
}

function mirror(messages: Row[]): PersistedState {
  return { messages, lastOrdinal: null, oldestOrdinal: null, hasMoreOlder: false };
}

async function pushFrame(frame: Record<string, unknown>): Promise<void> {
  await act(async () => {
    window.baybo.pushFrame(JSON.stringify(frame));
  });
  await settle();
}

async function settle(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 20));
  });
}

const cards = (): HTMLElement[] => [...document.querySelectorAll<HTMLElement>(".work-ladder")];
const liveCards = (): HTMLElement[] => [...document.querySelectorAll<HTMLElement>(".work.active")];
const pendingBox = (): HTMLElement | null => document.querySelector(".work-pending");

beforeEach(() => {
  vi.spyOn(console, "log").mockImplementation(() => {});
});

describe("a subscribe_state bundle and the settled turn above it", () => {
  // The gateway keeps reporting `turn.active` through post-answer finalization
  // and ships a rolling in-flight window with it. On a cold open the bundle's
  // turn identity has never been seen END here, so nothing upstream discards it:
  // the tail read is the only thing standing between it and the finished card.
  it("leaves the card of a turn whose answer is already on screen alone", async () => {
    mount(
      mirror([
        { id: "pm-1", role: "user", content: "explain" },
        {
          id: "w11",
          role: "work",
          steps: [{ kind: "tool", callId: "c1", label: "Read a.rs", status: "ok" }],
          active: false,
          elapsedMs: 12_000,
        },
        { id: "m11", role: "assistant", ordinal: 11, content: "here you go" },
      ]),
    );
    await settle();

    await pushFrame({
      kind: "subscribe_state",
      turn: { active: true, started_at: TURN_START },
      work_steps: [{ kind: "tool", call_id: "c9", tool: "Grep", label: "Grep zzz" }],
      pending_approvals: [],
    });

    // One card, still closed, still carrying the duration it was persisted with
    // — a rebuild would relight it (`active: true`) and clear `elapsedMs`, and
    // the collapsed summary is where both show.
    expect(cards()).toHaveLength(1);
    expect(liveCards()).toHaveLength(0);
    expect(cards()[0].textContent).toContain("12s");
    expect(cards()[0].textContent).not.toContain("Grep zzz");
  });

  // …but a trailing NOTICE is not an answer. It keeps its own row beside the
  // block it interrupted (`severTerminalNoticeIn`), and a tail read that stops at
  // it opens a second card below the notice instead of extending the live one.
  it("still rebuilds the live block sitting behind a terminal notice", async () => {
    mount(null);
    await pushFrame({ kind: "turn_state", active: true, started_at: TURN_START });
    await pushFrame({ kind: "reasoning", text: "weighing the options" });
    await pushFrame({ kind: "notice", text: "a tool failed", level: "error", mid_turn: false });
    expect(cards()).toHaveLength(1);

    await pushFrame({
      kind: "subscribe_state",
      turn: { active: true, started_at: TURN_START },
      work_steps: [
        { kind: "reasoning", text: "weighing the options" },
        { kind: "tool", call_id: "c9", tool: "Grep", label: "Grep zzz" },
      ],
      pending_approvals: [],
    });

    expect(cards()).toHaveLength(1);
    expect(cards()[0].textContent).toContain("Grep zzz");
    // The block is live again behind the notice — so the pending box, which
    // exists only to cover the gap BEFORE a turn's first frame, must not paint a
    // second "Working" line under it.
    expect(pendingBox()).toBeNull();
  });
});

describe("the working box retires with the turn", () => {
  // `turnActive` is the one run-state signal with no self-healing cap: the frame
  // that clears it is exactly the frame an offscreen buffer overflow drops, and
  // no `sync_page` carries turn state to rebuild it. Left to the server alone it
  // paints a spinner under the settled reply for the rest of the session.
  it("clears on the turn's own answer, without waiting for turn_state", async () => {
    mount(null);
    await pushFrame({ kind: "turn_state", active: true, started_at: TURN_START });
    expect(pendingBox()).not.toBeNull();
    expect(pendingBox()?.className).not.toContain("reserved");

    await pushFrame({ kind: "message", role: "assistant", ordinal: 12, content: "done" });

    expect(pendingBox()).toBeNull();
  });
});

describe("a read-only transcript's unanswered tail", () => {
  const baseline = (withOutput: boolean): Record<string, unknown> => ({
    kind: "sync_page",
    since_ordinal: null,
    next_cursor: withOutput ? 6 : 5,
    rebased: false,
    oldest_ordinal: 1,
    has_more_older: false,
    compaction_points: [{ ordinal: 4, at: "2026-08-16T12:00:04.000Z" }],
    rows: [
      {
        id: "m1",
        ordinal: 1,
        kind: "message",
        role: "user",
        text: "the errand",
      },
      {
        id: "w2",
        ordinal: 2,
        kind: "work",
        steps: [
          {
            kind: "tool",
            call_id: "c-before",
            tool: "bash",
            tool_label: "before compact",
            tool_status: "ok",
          },
        ],
        turn_complete: false,
      },
      {
        id: "w5",
        ordinal: 5,
        kind: "work",
        steps: [
          {
            kind: "tool",
            call_id: "c-after",
            tool: "bash",
            tool_label: "after compact",
            tool_status: "ok",
          },
        ],
        turn_complete: true,
      },
      ...(withOutput
        ? [
            {
              id: "m6",
              ordinal: 6,
              kind: "message",
              role: "assistant",
              text: "done",
            },
          ]
        : []),
    ],
  });

  it("opens every compact-split work half when no final output follows", async () => {
    mount(null, true);
    await pushFrame(baseline(false));

    expect(cards()).toHaveLength(2);
    expect(document.querySelectorAll(".work-steps")).toHaveLength(2);
    expect(document.body.textContent).toContain("before compact");
    expect(document.body.textContent).toContain("after compact");
    expect(document.querySelector(".compaction-divider")?.textContent).toContain("Compacted");
  });

  it("keeps the same work collapsed once a final output is present", async () => {
    mount(null, true);
    await pushFrame(baseline(true));

    expect(cards()).toHaveLength(2);
    expect(document.querySelectorAll(".work-steps")).toHaveLength(0);
    expect(document.body.textContent).toContain("done");
  });
});
