import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";

// bridge.ts reads `window.webkit` at module scope; MarkdownBody imports it for
// `openUrl`. jsdom has no bridge, but only link clicks touch it, so a stub keeps
// the import graph happy.
vi.mock("./bridge", () => ({ openUrl: vi.fn() }));

// jsdom has no ResizeObserver; the table renderer (TableBlock) constructs one in
// a mount effect. A no-op stub lets a table mount so the layout-integrity checks
// below can run.
vi.stubGlobal(
  "ResizeObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

import { MarkdownBody } from "./Markdown";

// The math pipeline (normalize -> remark-math -> rehype-katex -> KaTeX) is wired
// end to end here — a pure-function test can't prove the plugins are actually
// attached to <ReactMarkdown>. KaTeX builds plain DOM with inline styles (no
// layout/measurement), so it renders in jsdom. Like WorkBlock.test.tsx, this is
// a small presentational component with no scrollHeight/follow/pin dependency,
// so mounting it is fine (see app/ios/CLAUDE.md "web/").
describe("MarkdownBody math", () => {
  it("renders inline $...$ as KaTeX", () => {
    const { container } = render(<MarkdownBody text={"energy is $E = mc^2$ today"} />);
    expect(container.querySelector(".katex")).not.toBeNull();
    // The rendered math carries the source symbols.
    expect(container.textContent).toContain("E");
    expect(container.textContent).toContain("m");
  });

  it("renders a single-line $$...$$ as a KaTeX display block", () => {
    const { container } = render(<MarkdownBody text={"$$\\int_0^1 x\\,dx$$"} />);
    expect(container.querySelector(".katex-display")).not.toBeNull();
  });

  it("renders \\[...\\] as a KaTeX display block", () => {
    const { container } = render(<MarkdownBody text={"\\[a^2 + b^2 = c^2\\]"} />);
    expect(container.querySelector(".katex-display")).not.toBeNull();
  });

  it("renders the \\(...\\) delimiter form as inline KaTeX", () => {
    const { container } = render(<MarkdownBody text={"mass \\(a^2 + b^2\\) here"} />);
    expect(container.querySelector(".katex")).not.toBeNull();
    expect(container.querySelector(".katex-display")).toBeNull();
  });

  it("does not render math inside a code span", () => {
    const { container } = render(<MarkdownBody text={"literal `$x$` text"} />);
    expect(container.querySelector(".katex")).toBeNull();
    expect(container.querySelector("code")?.textContent).toBe("$x$");
  });

  it("does not throw on a malformed expression", () => {
    const { container } = render(<MarkdownBody text={"broken $\\frac{1}{$ end"} />);
    // No exception, and the surrounding prose still rendered.
    expect(container.textContent).toContain("end");
  });

  it("leaves paired currency as literal text, not math", () => {
    const { container } = render(<MarkdownBody text={"It costs $5 and $3 total."} />);
    expect(container.querySelector(".katex")).toBeNull();
    expect(container.textContent).toContain("$5");
    expect(container.textContent).toContain("$3");
  });

  it("still renders digit/decimal/arithmetic inline math (not mistaken for money)", () => {
    for (const src of ["$3.14$", "$2 + 2 = 4$", "$5 = x$"]) {
      const { container } = render(<MarkdownBody text={`value ${src} ok`} />);
      expect(container.querySelector(".katex")).not.toBeNull();
    }
  });

  it("keeps a numbered list intact when an item contains display math", () => {
    const { container } = render(
      <MarkdownBody text={"1. Plug in: $$x^2$$\n2. Simplify\n3. Done"} />,
    );
    // The promotion must not fracture the list into multiple <ol>s.
    expect(container.querySelectorAll("ol")).toHaveLength(1);
    expect(container.querySelectorAll("ol > li")).toHaveLength(3);
    // The embedded math still renders (inline, not a display block).
    expect(container.querySelector(".katex")).not.toBeNull();
    expect(container.querySelector(".katex-display")).toBeNull();
  });

  it("keeps a GFM table intact when a cell contains math", () => {
    const md = "| a | b |\n| --- | --- |\n| $$x^2$$ | y |";
    const { container } = render(<MarkdownBody text={md} />);
    expect(container.querySelector("table")).not.toBeNull();
    expect(container.querySelectorAll("tbody td")).toHaveLength(2);
    expect(container.querySelector(".katex")).not.toBeNull();
  });
});
