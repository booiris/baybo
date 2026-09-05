import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import { IssuePage } from "./IssuePage";
import type { IssueDetail, IssueEvent } from "./types";


const posted: { type: string; agentId?: string }[] = [];

vi.mock("../bridge", async (importOriginal) => {
  const real = await importOriginal<typeof import("../bridge")>();
  return {
    ...real,
    postToNative: (message: { type: string; agentId?: string }) => posted.push(message),
  };
});

vi.mock("./generatedFace", () => ({
  botttsPng: (agentId: string) => Promise.resolve(`png-for-${agentId}`),
}));

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

const said: IssueEvent = {
  id: "e1",
  number: 7,
  actor: { kind: "agent", id: "a-dev", handle: "dev-1" },
  body: { kind: "comment", text: "hi" },
  created_at_ms: 1,
};

function deliver(people: IssuePayload["people"]): void {
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  act(() => {
    page.deliver({ issue: card, events: [said], runs: [], people });
  });
}

async function settle(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function faces(): (string | undefined)[] {
  return posted.filter((m) => m.type === "generatedFace").map((m) => m.agentId);
}

beforeEach(() => {
  posted.length = 0;
  render(
    <I18nextProvider i18n={i18n}>
      <IssuePage />
    </I18nextProvider>,
  );
});

describe("a faceless teammate gets the generated face", () => {
  it("draws and uploads one for an agent with no avatar", async () => {
    deliver({ "a-dev": { handle: "dev-1", monogram: "D1" } });
    await settle();

    expect(faces()).toEqual(["a-dev"]);
  });

  it("does it once per agent, however many deliveries arrive", async () => {
    deliver({ "a-dev": { handle: "dev-1", monogram: "D1" } });
    await settle();
    deliver({ "a-dev": { handle: "dev-1", monogram: "D1" } });
    await settle();

    expect(faces()).toEqual(["a-dev"]);
  });

  it("leaves an agent that already has one alone", async () => {
    deliver({ "a-dev": { handle: "dev-1", monogram: "D1", avatar: "sha256:aa.tok" } });
    await settle();

    expect(faces()).toEqual([]);
  });

  it("covers every teammate the card names, not just the one who spoke", async () => {
    deliver({
      "a-dev": { handle: "dev-1", monogram: "DE1" },
      "a-docs": { handle: "docs-1", monogram: "DO1", avatar: "sha256:bb.tok" },
      "a-lead": { handle: "lead", monogram: "LE" },
    });
    await settle();

    expect(faces().sort()).toEqual(["a-dev", "a-lead"]);
  });
});
