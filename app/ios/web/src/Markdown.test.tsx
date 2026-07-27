import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";

// bridge.ts reads `window.webkit` at module scope; MarkdownBody imports it for
// `openUrl`. jsdom has no bridge, but only link clicks touch it, so a stub keeps
// the import graph happy.
vi.mock("./bridge", () => ({ openUrl: vi.fn() }));

// Delegates to the real normalizer everywhere except one sentinel, so the math
// suite below still exercises the shipped code. The sentinel is how the
// boundary's coverage of the PRE-parse step is testable at all — today's
// `normalizeMath` is capped-regex string work that won't throw on demand.
// `vi.hoisted` because `vi.mock`'s factory is hoisted above every plain const.
const { NORMALIZER_CRASHER } = vi.hoisted(() => ({ NORMALIZER_CRASHER: "normalizer-boom" }));
vi.mock("./mathDelimiters", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./mathDelimiters")>();
  return {
    ...actual,
    normalizeMath: (src: string) => {
      if (src === NORMALIZER_CRASHER) throw new Error("normalizer blew up");
      return actual.normalizeMath(src);
    },
  };
});

import { MarkdownBody } from "./Markdown";

// The math pipeline (normalize -> remark-math -> rehype-katex -> KaTeX) is wired
// end to end here — a pure-function test can't prove the plugins are actually
// attached to <ReactMarkdown>. KaTeX builds plain DOM with inline styles (no
// layout/measurement), so it renders in jsdom. Like WorkBlock.test.tsx, this is
// a small presentational component with no scrollHeight/follow/pin dependency,
// so mounting it is fine (see app/ios/docs/testing.md "web/").
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

// CommonMark's flanking rule refuses `**标题：**内容`: full-width punctuation
// INSIDE the closing run and a CJK letter immediately outside it make the run
// neither left- nor right-flanking, so it cannot close — and Chinese prose has
// no space to separate the two with. The `cjk-friendly` remark plugins are what
// pair it. Nothing else notices if one of them is dropped, reordered ahead of
// `remarkGfm`, or silently no-ops after a micromark bump: the shapes below are
// the contract, and to an English-only reader the failure is invisible.
describe("MarkdownBody CJK emphasis", () => {
  it("bolds across full-width punctuation inside the closing run", () => {
    for (const src of [
      "**标题：**内容说明",
      "**注意。**继续",
      "**注意！**下一句",
      "**真的吗？**是的",
      "**一、**说明",
      "**其次；**再说",
      "**（备注）**说明",
      "他说**“很重要”**的事",
      "见**《书名》**一节",
      "**このため。**次の文",
      "**제목：**내용",
    ]) {
      const { container } = render(<MarkdownBody text={src} />);
      expect(container.querySelector("strong"), src).not.toBeNull();
      expect(container.textContent, src).not.toContain("*");
    }
  });

  it("italicizes the same shape", () => {
    const { container } = render(<MarkdownBody text={"*标题：*内容说明"} />);
    expect(container.querySelector("em")?.textContent).toBe("标题：");
  });

  // The strikethrough plugin REPLACES remark-gfm's `~` construct rather than
  // layering over it, so this case is also the plugin-order canary: put it
  // ahead of `remarkGfm` and only CJK `~~` stops pairing — plain `~~gone~~`
  // keeps working, and nothing else in either suite moves.
  it("strikes the same shape, and leaves ASCII strikethrough working", () => {
    const cjk = render(<MarkdownBody text={"~~注意：~~内容"} />);
    expect(cjk.container.querySelector("del")?.textContent).toBe("注意：");
    const ascii = render(<MarkdownBody text={"~~gone~~ text"} />);
    expect(ascii.container.querySelector("del")?.textContent).toBe("gone");
  });

  it("bolds inside a list item", () => {
    const { container } = render(
      <MarkdownBody text={"- **要点：**说明文字\n- **其次：**补充说明"} />,
    );
    expect(container.querySelectorAll("li strong")).toHaveLength(2);
    expect(container.textContent).not.toContain("*");
  });

  it("bolds and strikes inside a GFM table cell", () => {
    const md = "| 项目 | 状态 |\n| --- | --- |\n| **要点：**说明 | ~~取消：~~完成 |";
    const { container } = render(<MarkdownBody text={md} />);
    expect(container.querySelector("tbody td strong")?.textContent).toBe("要点：");
    expect(container.querySelector("tbody td del")?.textContent).toBe("取消：");
  });

  it("bolds a label that introduces math", () => {
    const { container } = render(<MarkdownBody text={"**公式：**$x^2$ 就是这样"} />);
    expect(container.querySelector("strong")?.textContent).toBe("公式：");
    expect(container.querySelector(".katex")).not.toBeNull();
  });

  it("leaves ASCII CommonMark exactly as it was", () => {
    // No CJK anywhere, so the flanking rule still refuses it — the plugins must
    // not make emphasis greedier for English…
    const glued = render(<MarkdownBody text={"**Title:**content"} />);
    expect(glued.container.querySelector("strong")).toBeNull();
    // …while the spaced form English is actually written in still bolds.
    const spaced = render(<MarkdownBody text={"**Title:** content"} />);
    expect(spaced.container.querySelector("strong")?.textContent).toBe("Title:");
  });

  it("keeps stray asterisks, underscores and tildes literal", () => {
    for (const src of [
      "计算 2**3 等于 8",
      "用 *args 和 **kwargs 传参",
      "变量 foo_bar_baz",
      "范围是 1~10 之间",
    ]) {
      const { container } = render(<MarkdownBody text={src} />);
      expect(container.querySelector("strong"), src).toBeNull();
      expect(container.querySelector("em"), src).toBeNull();
      expect(container.querySelector("del"), src).toBeNull();
    }
  });

  it("leaves an unclosed ** literal (the mid-stream state)", () => {
    const { container } = render(<MarkdownBody text={"**标题：内容还没写完"} />);
    expect(container.querySelector("strong")).toBeNull();
    expect(container.textContent).toContain("**");
  });

  it("does not emphasize inside a code span", () => {
    const { container } = render(<MarkdownBody text={"写成 `**标题：**内容`"} />);
    expect(container.querySelector("strong")).toBeNull();
    expect(container.querySelector("code")?.textContent).toBe("**标题：**内容");
  });

  // Two shapes the plugins deliberately do NOT reach, pinned so an upstream
  // change flips a test instead of the transcript.
  it("leaves CJK underscore emphasis literal (the intraword `_` rule is kept)", () => {
    // Relaxing `_` too would start eating `snake_case`; assistants emit `**`
    // far more often, and this degrades to a visible `_`, never a mis-pairing.
    for (const src of ["这是_强调_内容", "这是__加粗__内容"]) {
      const { container } = render(<MarkdownBody text={src} />);
      expect(container.querySelector("em"), src).toBeNull();
      expect(container.querySelector("strong"), src).toBeNull();
    }
  });

  it("leaves an astral emoji glued to a Latin letter unpaired", () => {
    // The extension resolves surrogate pairs before classifying, so U+1F600 is
    // finally seen as the punctuation category CommonMark says it is — the
    // spec-correct read, at the cost of this one shape that used to pair by
    // accident. The CJK side of it is exactly what the plugins buy.
    const latin = render(<MarkdownBody text={"**done 😀**text"} />);
    expect(latin.container.querySelector("strong")).toBeNull();
    const cjk = render(<MarkdownBody text={"**完成 😀**继续"} />);
    expect(cjk.container.querySelector("strong")?.textContent).toBe("完成 😀");
  });
});

// `breaks` is opt-in, and the two callers must stay on opposite sides of it: an
// answer is written in paragraphs and wants CommonMark's softbreak fold, a
// reasoning trace is written as short lines and is destroyed by it.
describe("MarkdownBody breaks", () => {
  it("folds a soft line break by default", () => {
    const { container } = render(<MarkdownBody text={"第一行\n第二行"} />);
    expect(container.querySelector("br")).toBeNull();
    expect(container.querySelectorAll("p")).toHaveLength(1);
  });

  it("hardens a soft line break when asked", () => {
    const { container } = render(<MarkdownBody text={"第一行\n第二行"} breaks />);
    expect(container.querySelector("br")).not.toBeNull();
  });

  it("still pairs CJK emphasis with breaks on", () => {
    const { container } = render(<MarkdownBody text={"**要点：**说明\n下一行"} breaks />);
    expect(container.querySelector("strong")?.textContent).toBe("要点：");
  });

  it("leaves a blank-line paragraph split alone either way", () => {
    for (const breaks of [false, true]) {
      const { container } = render(<MarkdownBody text={"一段\n\n二段"} breaks={breaks} />);
      expect(container.querySelectorAll("p"), String(breaks)).toHaveLength(2);
    }
  });
});

// react-markdown parses inside `render`, so an uncaught throw takes the whole
// React root with it — a blank transcript. KaTeX supplies a real one: a lone low
// surrogate in a math span (what a slice at a UTF-16 boundary leaves) raises
// `RangeError: Invalid code point`.
describe("MarkdownBody failure fallback", () => {
  it("falls back to the raw source instead of taking the tree down", () => {
    const crasher = "$\uDC00\uDC00$";
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const { container } = render(<MarkdownBody text={crasher} />);
      expect(container.querySelector(".md-failed")?.textContent).toBe(crasher);
    } finally {
      spy.mockRestore();
    }
  });

  it("recovers on the next text, rather than pinning the row to plain text", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const { container, rerender } = render(<MarkdownBody text={"$\uDC00\uDC00$"} />);
      expect(container.querySelector(".md-failed")).not.toBeNull();
      rerender(<MarkdownBody text={"**要点：**说明"} />);
      expect(container.querySelector(".md-failed")).toBeNull();
      expect(container.querySelector("strong")?.textContent).toBe("要点：");
    } finally {
      spy.mockRestore();
    }
  });

  // `normalizeMath` runs BEFORE the parse and over the same damaged text, so it
  // has to sit under the boundary too. Called from `MarkdownBody`'s own render
  // it would sit in the boundary's PARENT, and a throw would take the whole
  // tree down exactly as if there were no boundary at all.
  it("catches a throw from the pre-parse normalizer, not just from the parse", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const { container } = render(<MarkdownBody text={NORMALIZER_CRASHER} />);
      expect(container.querySelector(".md-failed")?.textContent).toBe(NORMALIZER_CRASHER);
    } finally {
      spy.mockRestore();
    }
  });
});
