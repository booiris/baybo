// Ported verbatim from `app/mobile/web/src/mathDelimiters.test.ts`, alongside the
// module it covers — the two clients must agree case for case. Kept a byte copy
// past this header; `mathDelimiters.port.test.ts` enforces that.
import { describe, expect, it } from "vitest";

import { normalizeMath } from "./mathDelimiters";

describe("normalizeMath", () => {
  it("fast-returns text with no dollar or AMS delimiter", () => {
    expect(normalizeMath("no math here")).toBe("no math here");
    expect(normalizeMath("a list:\n- one\n- two")).toBe("a list:\n- one\n- two");
  });

  it("keeps a valid inline $...$ span untouched, including digit/decimal/arithmetic math", () => {
    for (const s of ["$x^2$", "$a_i$", "$3.14$", "$2 + 2 = 4$", "$5 = x$", "$0$", "$2\\pi$"]) {
      expect(normalizeMath(`math ${s} here`)).toBe(`math ${s} here`);
    }
  });

  it("escapes prose money so it is not paired into inline math", () => {
    expect(normalizeMath("It costs $5 and $3 total.")).toBe("It costs \\$5 and \\$3 total.");
    expect(normalizeMath("was $12.50, now $9")).toBe("was \\$12.50, now \\$9");
    expect(normalizeMath("Between $100 and $1,999.99 each.")).toBe(
      "Between \\$100 and \\$1,999.99 each.",
    );
  });

  it("escapes money that PRECEDES a real math span on the same line", () => {
    // money's `$` must not pair across a later `$x$` (the money-before-math trap)
    expect(normalizeMath("$5 and $x$")).toBe("\\$5 and $x$");
    expect(normalizeMath("It costs $5, then $x$ later")).toBe("It costs \\$5, then $x$ later");
    expect(normalizeMath("$100 and $200 and $x$")).toBe("\\$100 and \\$200 and $x$");
    expect(normalizeMath("$E=mc^2$ costs $5")).toBe("$E=mc^2$ costs \\$5");
  });

  it("escapes money that precedes a stray or unclosed $ (not a real closer)", () => {
    expect(normalizeMath("Prices range from $5 to $N per unit.")).toBe(
      "Prices range from \\$5 to \\$N per unit.",
    );
    expect(normalizeMath("It costs $5, and the $ means dollars.")).toBe(
      "It costs \\$5, and the \\$ means dollars.",
    );
    expect(normalizeMath("keep digit math $3.14$ and $2\\pi$ intact")).toBe(
      "keep digit math $3.14$ and $2\\pi$ intact",
    );
  });

  it("converts inline \\(...\\) to single-dollar", () => {
    expect(normalizeMath("mass-energy \\(E = mc^2\\) rules")).toBe("mass-energy $E = mc^2$ rules");
  });

  it("converts a standalone \\[...\\] to a block $$ on its own lines", () => {
    expect(normalizeMath("\\[\\int_0^1 x\\,dx\\]")).toBe("$$\n\\int_0^1 x\\,dx\n$$");
  });

  it("promotes a standalone single-line $$...$$ onto its own lines (display)", () => {
    expect(normalizeMath("$$E=mc^2$$")).toBe("$$\nE=mc^2\n$$");
    expect(normalizeMath("intro\n\n$$ E=mc^2 $$\n\noutro")).toBe("intro\n\n$$\nE=mc^2\n$$\n\noutro");
  });

  it("does NOT promote a $$ embedded in a list item or indented (keeps structure)", () => {
    expect(normalizeMath("1. Plug in: $$x^2$$\n2. Next step")).toBe(
      "1. Plug in: $$x^2$$\n2. Next step",
    );
    expect(normalizeMath("- item\n  $$x^2$$")).toBe("- item\n  $$x^2$$");
  });

  it("does not touch $ / \\( / \\[ inside a fenced code block (any indent, nested lists)", () => {
    const top = "```js\nconst re = /\\(/; // $$x$$ and $5 and $3\n```\nthen \\(z\\)";
    expect(normalizeMath(top)).toBe("```js\nconst re = /\\(/; // $$x$$ and $5 and $3\n```\nthen $z$");
    // a fence nested >3 spaces inside a list must still be masked
    const nested = "- a\n  - b\n    ```\n    echo \"$5 and $3\"\n    ```";
    expect(normalizeMath(nested)).toBe(nested);
  });

  it("masks an UNCLOSED fence to end-of-input (streaming / malformed)", () => {
    const src = "before \\(a\\)\n```\n$$x$$ and \\(b\\) and $5 and $9";
    expect(normalizeMath(src)).toBe("before $a$\n```\n$$x$$ and \\(b\\) and $5 and $9");
  });

  it("does not touch delimiters inside an inline code span", () => {
    expect(normalizeMath("use \\(x\\) not `\\(y\\)`")).toBe("use $x$ not `\\(y\\)`");
  });

  it("leaves an unclosed delimiter alone (streaming partial)", () => {
    expect(normalizeMath("half open \\(E = mc")).toBe("half open \\(E = mc");
    expect(normalizeMath("price is $5 with no")).toBe("price is \\$5 with no");
  });

  it("leaves a source-borne mask sentinel alone (no 'undefined' splice)", () => {
    // The masking pass brackets its placeholder indices with NUL/SOH on the
    // assumption they cannot occur in transcript text. A message that carries
    // them anyway used to unmask an index nobody minted and splice the string
    // "undefined" over the author's characters.
    const nul = String.fromCharCode(0);
    const soh = String.fromCharCode(1);
    expect(normalizeMath(`cost $5 and ${nul}0${soh} tail`)).toBe(
      `cost \\$5 and ${nul}0${soh} tail`,
    );
  });

  it("does not hang on adversarial input (no super-linear time)", () => {
    // These run ~1-30ms when linear; a quadratic regression blows to seconds.
    // The bound is generous so CI/JIT noise can't flake it while still catching
    // that class of blowup.
    for (const src of ["$$x$$ " + "`".repeat(50000), "\\[".repeat(50000), "$1 ".repeat(50000)]) {
      const t = performance.now();
      normalizeMath(src);
      expect(performance.now() - t).toBeLessThan(1000);
    }
  });
});
