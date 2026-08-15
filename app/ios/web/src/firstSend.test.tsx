import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { Transcript } from "./Transcript";

/// The first send of a NEW conversation, driven through the mounted component —
/// because the hole this pins lived between two units that were each "correct":
/// `handleUserSent` arms the REPLACE-overlay's kept set, and `applySyncReplace`
/// honours it. But when the optimistic `userSent` is LOST (a cross-session
/// retarget burst consumed by the outgoing tree — see bridge.test.ts), the row's
/// only origin is the frame path's echo append, which never armed the set — so
/// the send-time baseline REPLACE, routinely served before the gateway persisted
/// the row, deleted the one rendering the message had. No pure-reducer suite can
/// see that: the membership add and the row append live in the component.
///
/// No fake layout here (see transcriptScroll.test.tsx for why one exists): every
/// scroll branch degenerates to a no-op in jsdom, and nothing below reads
/// geometry — only which rows are in the tree.

vi.stubGlobal(
  "IntersectionObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

function rowIds(): string[] {
  return [...document.querySelectorAll("[data-row-id]")].map(
    (el) => el.getAttribute("data-row-id") ?? "",
  );
}

async function mountFresh(): Promise<void> {
  render(
    <I18nextProvider i18n={i18n}>
      <Transcript restored={null} initialConnEpoch={0} />
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

async function settle(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 20));
  });
}

beforeEach(() => {
  vi.spyOn(console, "log").mockImplementation(() => {});
});

describe("an echo-appended first send survives the raced baseline", () => {
  it("enrols the ordinal-less echo row in the kept set, so a page without it cannot delete it", async () => {
    await mountFresh();

    // The echo: ordinal-less (`channel/adapter.rs` hardcodes None), and the
    // pmid is unknown here — the optimistic bubble never rendered.
    await pushFrame({
      kind: "message",
      role: "user",
      content: "hi",
      platform_msg_id: "pm-1",
    });
    expect(rowIds()).toContain("pm-1");

    // The reply lands live first and advances the cursor past the still
    // ordinal-less user row.
    await pushFrame({
      kind: "message",
      role: "assistant",
      ordinal: 12,
      content: "there",
    });

    // A baseline REPLACE that does not carry the user row. (The routine raced
    // shape is `rows: []`, which `applySyncReplace` now refuses outright; this
    // page also pins the membership itself, which any other narrow REPLACE
    // shape relies on.)
    await pushFrame({
      kind: "sync_page",
      since_ordinal: null,
      rebased: false,
      rows: [
        {
          id: "m12",
          ordinal: 12,
          kind: "message",
          role: "assistant",
          text: "there",
          created_at: "2026-08-15T00:00:00Z",
        },
      ],
      next_cursor: 12,
      oldest_ordinal: 12,
      has_more_older: false,
      compaction_points: [],
    });

    // Exact order: keptSends returns behind the page, so the survivor sits
    // BELOW the reply here. Acceptable because this page shape is artificial —
    // a persisted reply implies its user row persisted first, so a real
    // narrow page carries both, and the real raced shape is the EMPTY page,
    // which applySyncReplace now refuses outright. The pin is survival; any
    // placement change must be a conscious edit of this line.
    expect(rowIds()).toEqual(["m12", "pm-1"]);
  });
});
