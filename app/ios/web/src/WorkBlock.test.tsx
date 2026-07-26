import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
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
  const { container } = render(
    <I18nextProvider i18n={i18n}>
      <WorkBlockView row={row} onToggle={onToggle} />
    </I18nextProvider>,
  );
  return { onToggle, container };
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

// Which steps are MARKDOWN and which are verbatim is a contract shared with the
// web chat (`app/web/src/pages/workStep.test.tsx`, whose `WorkStepView` is a
// hand-duplicate of this one): the model writes its reasoning in the same
// markdown as its answer, so raw text leaks `**` to the reader, while a status
// line and a notice are our own strings and must never be re-parsed.
describe("WorkBlockView — step markup", () => {
  it("renders reasoning and folded prose as markdown", () => {
    const { container } = renderWork(
      workRow({
        active: true,
        steps: [
          { kind: "reasoning", text: "**要点：**先看配置" },
          { kind: "prose", text: "**结论：**可以上线" },
        ],
      }),
    );
    expect(container.querySelector(".step.reasoning strong")?.textContent).toBe("要点：");
    expect(container.querySelector(".step.prose strong")?.textContent).toBe("结论：");
    expect(container.textContent).not.toContain("*");
  });

  // The trace is written as short newline-separated lines. CommonMark folds a
  // single newline into a space, which ran the whole trace together the moment
  // it started being parsed; `breaks` is what buys the shape back. Folded prose
  // is answer text and deliberately stays on the paragraph rule.
  it("keeps the trace line-broken, while folded prose stays folded", () => {
    const { container } = renderWork(
      workRow({
        active: true,
        steps: [
          { kind: "reasoning", text: "看配置\n看日志\n看指标" },
          { kind: "prose", text: "第一行\n第二行" },
        ],
      }),
    );
    expect(container.querySelectorAll(".step.reasoning br")).toHaveLength(2);
    expect(container.querySelector(".step.prose br")).toBeNull();
  });

  // `TableBlock` builds a ResizeObserver in a mount effect, which jsdom lacks —
  // and `MarkdownFallback` now CATCHES that, so without the stub in
  // `test/setup.ts` a table degrades to `.md-failed` raw source and every
  // assertion around it still passes. Pin the visible half of that.
  it("renders a table in the trace as a table, not as fallback source", () => {
    const { container } = renderWork(
      workRow({
        active: true,
        steps: [{ kind: "reasoning", text: "| 项目 | 状态 |\n| --- | --- |\n| 配置 | 完成 |" }],
      }),
    );
    expect(container.querySelector(".step.reasoning table")).not.toBeNull();
    expect(container.querySelector(".md-failed")).toBeNull();
  });

  it("leaves a status line and a notice verbatim", () => {
    const { container } = renderWork(
      workRow({
        active: true,
        steps: [
          { kind: "status", text: "reading **config.toml**" },
          { kind: "notice", level: "warn", text: "context **compacted**" },
        ],
      }),
    );
    expect(container.querySelector("strong")).toBeNull();
    expect(container.textContent).toContain("**config.toml**");
    expect(container.textContent).toContain("**compacted**");
  });
});

// A live trace grows by a chunk per frame with nothing pacing it, and each frame
// re-parses the WHOLE accumulated text — quadratic in the trace's length.
// `useSampledText` caps the parse rate; what must NOT regress is that the last
// chunk still lands. Fake timers here: the sampler is wall-clock, not rAF.
describe("WorkBlockView — reasoning sampling", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("paints the first chunk immediately, then settles on the final text", () => {
    vi.useFakeTimers();
    const row = (text: string) => workRow({ active: true, steps: [{ kind: "reasoning", text }] });
    const { container, rerender } = render(
      <I18nextProvider i18n={i18n}>
        <WorkBlockView row={row("first")} onToggle={vi.fn()} />
      </I18nextProvider>,
    );
    expect(container.textContent).toContain("first");

    for (const text of ["first s", "first se", "first sec", "first second"]) {
      rerender(
        <I18nextProvider i18n={i18n}>
          <WorkBlockView row={row(text)} onToggle={vi.fn()} />
        </I18nextProvider>,
      );
    }
    expect(container.textContent).not.toContain("second");

    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(container.textContent).toContain("first second");
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
