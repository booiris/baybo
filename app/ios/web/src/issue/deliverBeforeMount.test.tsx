import { act } from "@testing-library/react";
import { afterAll, beforeEach, expect, it } from "vitest";

import i18n from "../i18n";
import type { IssuePayload } from "./bridge";
import type { IssueDetail } from "./types";

/// The card native ALREADY has must reach a page that has not mounted yet.
///
/// This is the whole open path, not a cold-start corner. `main.tsx` posts
/// `issueReady` on the line after `createRoot().render(…)` — React has only
/// SCHEDULED the tree at that point — and native answers in the same main-actor
/// turn with everything it is holding: the flushed `pending` evals plus the
/// `redeliver()` its ready handler runs. On a directly-connected gateway (or
/// off a mirror that was on disk before the webview existed) that is the whole
/// card, ~18 ms before the tree commits.
///
/// So the delivery lands with nothing subscribed, and the buffer is what
/// carries it across. What broke it was `main.tsx` taking the subscription slot
/// with a language stub whose `deliver` was `() => undefined`: the buffer only
/// holds while the slot is EMPTY, so the card was handed to the stub, dropped,
/// and never sent again — the page sat on "Loading card…" with the card
/// already in the app. The relay leg hid it by being slow enough that the
/// fetch landed after the tree had subscribed.
///
/// The real entry module is imported rather than `<IssuePage/>` mounted by
/// hand, because the defect was in the WIRING: any test that mounts the page
/// first passes on the broken code.

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

  // Native's answer to `issueReady`, in the turn the page reported it — before
  // React has committed anything.
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  page.init({ language: "en", projectId: "01JBOARD", number: 26, bottomInset: 24 });
  page.deliver(payload);

  await act(async () => undefined);

  expect(document.querySelector(".issue-loading")).toBeNull();
  expect(document.body.textContent).toContain("Implement Go document structure");
});

/// The other half of the same move: language still reaches i18n.
///
/// It used to ride the subscription slot, which is why it was parked there in
/// the first place — and it was ALREADY broken there, silently: `IssuePage`
/// replaces the slot the moment it mounts, so every switch after the first
/// paint went to a handler whose comment said main.tsx owned it. On its own
/// listener both are true at once.
it("still switches the language native asks for, after the page has mounted", async () => {
  await import("./main");
  const page = window.issuePage;
  if (page === undefined) throw new Error("window.issuePage missing");
  page.init({ language: "en", projectId: "01JBOARD", number: 26, bottomInset: 0 });
  await act(async () => undefined);

  page.setLanguage("zh");
  await act(async () => undefined);

  expect(i18n.language).toBe("zh");
});

afterAll(async () => {
  await i18n.changeLanguage("en");
});
