import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

vi.stubGlobal(
  "IntersectionObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

import { Transcript } from "./Transcript";
import i18n from "./i18n";

const init = (sessionId: string) =>
  window.baybo.init({
    language: "en",
    sessionId,
    restoredState: null,
    connEpoch: 0,
    expandUnansweredTail: false,
  });

// The cross-session retarget veil (bridge.ts concealForRetarget): the moment a
// DIFFERENT session's init lands, the old conversation is hidden by a
// synchronous DOM write; the incoming tree lifts the veil from its mount
// layout effect, in the same frame its own content paints. A same-session
// re-init must NOT conceal — nothing remounts, so nothing would ever reveal
// (the failsafe would, but only after a visible blank).
describe("the retarget veil", () => {
  it("conceals on a cross-session init and reveals on the incoming mount", () => {
    document.body.innerHTML = '<div id="root"></div>';
    const html = document.documentElement;
    init("session-a");
    expect(html.classList.contains("retargeting")).toBe(false);
    init("session-b");
    expect(html.classList.contains("retargeting")).toBe(true);
    render(
      <I18nextProvider i18n={i18n}>
        <Transcript restored={null} initialConnEpoch={0} />
      </I18nextProvider>,
    );
    expect(html.classList.contains("retargeting")).toBe(false);
  });

  it("does not conceal a same-session re-init", () => {
    init("session-b");
    expect(document.documentElement.classList.contains("retargeting")).toBe(false);
  });
});
