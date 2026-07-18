import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { WorkBlockView } from "./WorkBlock";
import type { WorkRow, WorkStep } from "./types";

// The one render test in this bundle. WorkBlockView is a small presentational
// card with no scrollHeight / follow / pin logic, so it renders cleanly in
// jsdom — unlike <Transcript>, which is why the rest of the suite stays pure
// (see app/ios/CLAUDE.md "web/"). Its pure helpers are covered by
// formatters.test.ts; this pins the component wiring: active vs closed, the
// expand toggle, and the step feed. Text is the real en locale via i18n.

const toolStep = (over: Partial<Extract<WorkStep, { kind: "tool" }>> = {}): WorkStep => ({
  kind: "tool",
  callId: "c1",
  label: "Bash",
  status: "ok",
  ...over,
});

const workRow = (over: Partial<WorkRow> = {}): WorkRow => ({
  id: "w1",
  role: "work",
  steps: [],
  active: false,
  ...over,
});

function renderWork(row: WorkRow, onToggle = vi.fn()) {
  render(
    <I18nextProvider i18n={i18n}>
      <WorkBlockView row={row} onToggle={onToggle} />
    </I18nextProvider>,
  );
  return { onToggle };
}

describe("WorkBlockView — active", () => {
  it("shows the working header and the live step feed", () => {
    renderWork(
      workRow({
        active: true,
        steps: [toolStep({ label: "Bash", summary: "ls" }), { kind: "status", text: "scanning" }],
      }),
    );
    expect(screen.getByText("Working")).toBeInTheDocument();
    expect(screen.getByText(/Bash/)).toBeInTheDocument();
    expect(screen.getByText("scanning")).toBeInTheDocument();
  });

  it("renders a blocked tool step's approval label", () => {
    renderWork(
      workRow({ active: true, steps: [toolStep({ status: "running", awaitingApproval: "p1" })] }),
    );
    expect(screen.getByText("waiting for approval")).toBeInTheDocument();
  });
});

describe("WorkBlockView — closed", () => {
  it("shows a 'Worked' summary with steps hidden until the user taps it", async () => {
    const user = userEvent.setup();
    const { onToggle } = renderWork(
      workRow({ active: false, elapsedMs: 5000, steps: [{ kind: "status", text: "done scanning" }] }),
    );
    const summary = screen.getByRole("button");
    expect(summary).toHaveTextContent("Worked 5s");
    expect(screen.queryByText("done scanning")).toBeNull();

    await user.click(summary);
    expect(onToggle).toHaveBeenCalledWith(true);
    expect(screen.getByText("done scanning")).toBeInTheDocument();
  });
});
