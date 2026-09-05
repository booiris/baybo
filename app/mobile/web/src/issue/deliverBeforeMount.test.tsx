import { act } from "@testing-library/react";
import { afterAll, beforeEach, expect, it } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import type { IssueDetail } from "./types";

// createRoot().render only schedules React; native can deliver in the same turn
// before the subscription exists. This exercises the real entry wiring so that
// pre-mount payload must survive in the bridge buffer.
const card: IssueDetail = {
  number: 26,
  project_id: "01JBOARD",
  title: "Implement Go document structure",
  description: "",
  status: "review",
  priority: "none",
  position: 0,
  pinned: false,
  stage: 3,
  unread: 0,
  last_run_failed: false,
  approval_pending: false,
  opened_by_agent: false,
  branch: "issue-26",
  created_at_ms: 1,
  updated_at_ms: 9,
};

const payload: IssuePayload = {
  issue: card,
  events: [],
  runs: [],
  people: { "a-dev": { handle: "dev-1", monogram: "D1" } },
};

beforeEach(() => {
  document.body.innerHTML = '<div id="issue-root"></div>';
});

it("replays the card that landed while the tree was still being scheduled", async () => {
  await import("./main");

  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  page.init({
    language: "en",
    projectId: "01JBOARD",
    number: 26,
    targetId: "visit-26",
    bottomInset: 24,
  });
  page.deliver(payload);

  await act(async () => undefined);

  expect(document.querySelector(".issue-loading")).toBeNull();
  expect(document.body.textContent).toContain("Implement Go document structure");

  const root = document.getElementById("issue-root");
  if (root === null) throw new Error("issue-root missing");
  let sawLoading = false;
  const observer = new MutationObserver(() => {
    sawLoading ||= root.querySelector(".issue-loading") !== null;
  });
  observer.observe(root, { childList: true, subtree: true });

  await act(async () => {
    page.init({
      language: "en",
      projectId: "01JBOARD",
      number: 27,
      targetId: "visit-27",
      bottomInset: 24,
    });
  });
  expect(root).toHaveClass("issue-retargeting");
  expect(root.querySelector(".issue-loading")).toBeNull();

  await act(async () => {
    page.deliver({
      ...payload,
      issue: { ...card, number: 27, title: "Retarget the warm issue renderer" },
    });
  });
  observer.disconnect();

  expect(sawLoading).toBe(false);
  expect(root).not.toHaveClass("issue-retargeting");
  expect(document.body.textContent).toContain("Retarget the warm issue renderer");
  expect(document.body.textContent).not.toContain("Implement Go document structure");
});

it("still switches the language native asks for, after the page has mounted", async () => {
  await import("./main");
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  page.init({
    language: "en",
    projectId: "01JBOARD",
    number: 26,
    targetId: "visit-26-language",
    bottomInset: 0,
  });
  await act(async () => undefined);

  page.setLanguage("zh");
  await act(async () => undefined);

  expect(i18n.language).toBe("zh");
});

afterAll(async () => {
  await i18n.changeLanguage("en");
});
