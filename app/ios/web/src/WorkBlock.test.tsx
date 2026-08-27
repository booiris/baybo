import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { compactionStatusText, segmentWorkSteps, WorkBlockView } from "./WorkBlock";
import type { WorkRow, WorkStep } from "./types";

// The one render test in this bundle. WorkBlockView is a small presentational
// card with no scrollHeight / follow / pin logic, so it renders cleanly in
// jsdom — unlike <Transcript>, which is why the rest of the suite stays pure
// (see app/ios/docs/testing.md "web/"). Its pure helpers are covered by
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

function renderWork(row: WorkRow, onToggle = vi.fn(), defaultExpanded = false) {
  const { container } = render(
    <I18nextProvider i18n={i18n}>
      <WorkBlockView row={row} defaultExpanded={defaultExpanded} onToggle={onToggle} />
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
    expect(container.querySelector(".work-said strong")?.textContent).toBe("结论：");
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
    expect(container.querySelector(".work-said br")).toBeNull();
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

const T0 = 1_700_000_000_000;
const at = (sec: number) => T0 + sec * 1000;

// A turn renders as a LADDER: one "Worked Xs ›" run per stretch of work, each
// timing itself from the model's previous remark to its next. Mirrors
// app/web/src/pages/workSegments.test.tsx.
describe("WorkBlockView — the ladder", () => {
  const headers = (c: HTMLElement) => [...c.querySelectorAll(".work-summary")].map((e) => e.textContent);
  // turn start ─12s─ "我先找一下" ─40s─ "找到了" ─8s─ turn end
  const LADDER: WorkStep[] = [
    { kind: "reasoning", text: "thinking", at: at(2) },
    { kind: "prose", text: "我先找一下", at: at(12) },
    toolStep({ callId: "c1", label: "Grep", at: at(20) }),
    { kind: "prose", text: "找到了", at: at(52) },
    toolStep({ callId: "c2", label: "Edit", at: at(55) }),
  ];
  const ladderRow = (over: Partial<WorkRow> = {}) =>
    workRow({ active: false, startedAt: at(0), elapsedMs: 60_000, steps: LADDER, ...over });

  it("gives every stretch of work its own header and its own duration", () => {
    const { container } = renderWork(ladderRow());
    expect(headers(container)).toEqual(["Worked 12s›", "Worked 40s›", "Worked 8s›"]);
    expect(screen.getByText("我先找一下")).toBeInTheDocument();
    expect(screen.getByText("找到了")).toBeInTheDocument();
    // Collapsed: every run's machinery is shut, every remark is on screen.
    expect(container.querySelectorAll(".work-steps")).toHaveLength(0);
  });

  // The shape a cold start hands this view: `elapsedMs` survives the mirror,
  // the anchor does not (stripped for a block still active at persist, and
  // absent outright on rows written before that strip narrowed). A turn with no
  // mid-turn remark is ONE run spanning the whole block, so `elapsedMs` is
  // exactly its span — reading "3 steps" there with the real number on the row
  // is the regression `3730466d` shipped.
  it("times a lone run from the block when the mirror dropped the anchor", () => {
    const { container } = renderWork(
      workRow({ steps: [{ kind: "reasoning", text: "thinking" }, toolStep(), toolStep({ callId: "c2" })], elapsedMs: 167_000 }),
    );
    expect(headers(container)).toEqual(["Worked 2m 47s›"]);
  });

  // No honest duration once the turn has remarks in it: the runs are each
  // SHORTER than the block, so the block's total belongs to none of them. Say
  // "Worked" and stop — a step count there answers a question nobody asked, and
  // it is what the run expands to show anyway.
  it("falls back to a bare Worked for a multi-run turn with no stamps", () => {
    const { container } = renderWork(
      workRow({ elapsedMs: 60_000, steps: LADDER.map(({ at: _at, ...s }) => s as WorkStep) }),
    );
    expect(headers(container)).toEqual(["Worked›", "Worked›", "Worked›"]);
  });

  it("opens one run without inserting the others", async () => {
    const user = userEvent.setup();
    const { container, onToggle } = renderWork(ladderRow());
    await user.click(container.querySelectorAll(".work-summary")[1]);
    expect(onToggle).toHaveBeenCalledWith(true);
    expect(container.querySelectorAll(".work-steps")).toHaveLength(1);
    expect(container.textContent).toContain("Grep");
    expect(container.textContent).not.toContain("thinking");
  });

  it("can open a closed run on its first paint without taking away the toggle", async () => {
    const user = userEvent.setup();
    const { container, onToggle } = renderWork(ladderRow(), vi.fn(), true);
    expect(container.querySelectorAll(".work-steps")).toHaveLength(3);
    expect(container.querySelectorAll(".work-chevron.open")).toHaveLength(3);

    await user.click(container.querySelectorAll(".work-summary")[1]);
    expect(onToggle).toHaveBeenCalledWith(false);
    expect(container.querySelectorAll(".work-steps")).toHaveLength(2);
  });

  it("only the LAST run of a live turn reads as running", () => {
    const { container } = renderWork(ladderRow({ active: true }));
    expect(headers(container)).toEqual(["Worked 12s›", "Worked 40s›"]);
    expect(screen.getByText("Working")).toBeInTheDocument();
  });

  it("marks only the run the stop landed on", () => {
    const { container } = renderWork(ladderRow({ cancelled: true }));
    expect(headers(container)[2]).toContain("Cancelled");
    expect(headers(container)[0]).toBe("Worked 12s›");
  });

  it("falls back to a bare Worked when the boundary carries no timestamp", () => {
    // A row a gateway predating `ChatWorkStep.at` reconstructed. The block's
    // 5s belongs to neither run — both are shorter — so neither claims it.
    const { container } = renderWork(
      workRow({
        active: false,
        elapsedMs: 5000,
        steps: [{ kind: "reasoning", text: "r" }, { kind: "prose", text: "说点什么" }, toolStep({})],
      }),
    );
    expect(headers(container)).toEqual(["Worked›", "Worked›"]);
  });

  it("a turn with no narration is a single run — the common shape", () => {
    const { container } = renderWork(
      workRow({ active: false, startedAt: at(0), elapsedMs: 5000, steps: [toolStep({})] }),
    );
    expect(headers(container)).toEqual(["Worked 5s›"]);
  });

  it("a block of pure narration has no header at all", () => {
    const { container } = renderWork(
      workRow({ active: false, elapsedMs: 5000, steps: [{ kind: "prose", text: "就这样", at: at(3) }] }),
    );
    expect(headers(container)).toEqual([]);
    expect(screen.getByText("就这样")).toBeInTheDocument();
  });

  // The point of the whole redesign: the collapse hides the MACHINERY. What the
  // agent actually said mid-turn stays on the page with no tap needed.
  it("keeps mid-turn narration visible in the collapsed block", () => {
    renderWork(
      workRow({
        active: false,
        startedAt: at(0),
        elapsedMs: 5000,
        steps: [{ kind: "prose", text: "先找一下 fold 在哪", at: at(2) }, { kind: "status", text: "done scanning" }],
      }),
    );
    expect(screen.getByText("先找一下 fold 在哪")).toBeInTheDocument();
    expect(screen.queryByText("done scanning")).toBeNull();
  });
});

describe("segmentWorkSteps — speech stays, machinery folds", () => {
  const say = (text: string): WorkStep => ({ kind: "prose", text });
  const shape = (steps: WorkStep[]) =>
    segmentWorkSteps(steps).map((s) => `${s.kind}:${s.steps.length}`);

  it("a turn with no narration is one machinery run — the common shape, unchanged", () => {
    expect(shape([{ kind: "reasoning", text: "r" }, toolStep({ callId: "c1" }), toolStep({ callId: "c2" })])).toEqual([
      "machinery:3",
    ]);
  });

  it("splits at every prose step, preserving chronological order", () => {
    expect(
      shape([
        { kind: "reasoning", text: "r1" },
        say("first"),
        toolStep({ callId: "c1" }),
        say("second"),
        toolStep({ callId: "c2" }),
      ]),
    ).toEqual(["machinery:1", "speech:1", "machinery:1", "speech:1", "machinery:1"]);
  });

  it("coalesces adjacent prose into ONE speech run", () => {
    expect(shape([say("a"), say("b"), toolStep({ callId: "c1" })])).toEqual(["speech:2", "machinery:1"]);
  });

  it("handles a block that leads with speech — the [Text, ToolUse] iteration shape", () => {
    expect(shape([say("a"), toolStep({ callId: "c1" })])).toEqual(["speech:1", "machinery:1"]);
  });

  it("handles a prose-only block (reachable via a mid-turn subscribe_state REPLACE)", () => {
    expect(shape([say("a")])).toEqual(["speech:1"]);
  });

  it("is empty for an empty block", () => {
    expect(shape([])).toEqual([]);
  });

  it("groups notices with the machinery they interrupt, not with speech", () => {
    expect(shape([say("a"), { kind: "notice", level: "info", text: "n" }, toolStep({ callId: "c1" })])).toEqual([
      "speech:1",
      "machinery:2",
    ]);
  });
});

describe("compactionStatusText", () => {
  const t = ((key: string) => key) as unknown as Parameters<typeof compactionStatusText>[0];

  it("names the two phases the server emits", () => {
    expect(compactionStatusText(t, "compacting")).toBe("chat.compacting");
    expect(compactionStatusText(t, "compacted")).toBe("chat.compacted");
  });

  it("shows an unknown phase raw rather than swallowing the turn's only explanation", () => {
    expect(compactionStatusText(t, "recompiling")).toBe("recompiling");
  });
});
