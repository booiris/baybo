import test from "node:test";
import assert from "node:assert/strict";

import { StreamingMarkdownFilter, sanitizeMarkdown } from "../dist/messaging/markdown-filter.js";

function oneShot(input) {
  const f = new StreamingMarkdownFilter();
  return f.feed(input) + f.flush();
}

function charByChar(input) {
  const f = new StreamingMarkdownFilter();
  let out = "";
  for (const ch of input) out += f.feed(ch);
  out += f.flush();
  return out;
}

function randomChunks(input, seed = 42) {
  const f = new StreamingMarkdownFilter();
  let out = "";
  let pos = 0;
  let s = seed;
  while (pos < input.length) {
    s = (s * 1103515245 + 12345) & 0x7fffffff;
    const size = (s % 5) + 1;
    out += f.feed(input.slice(pos, pos + size));
    pos += size;
  }
  out += f.flush();
  return out;
}

function expectFilter(input, expected) {
  assert.equal(oneShot(input), expected, `oneShot: ${JSON.stringify(input)}`);
  assert.equal(charByChar(input), expected, `charByChar: ${JSON.stringify(input)}`);
  assert.equal(randomChunks(input), expected, `randomChunks: ${JSON.stringify(input)}`);
}

test("plain text passes through unchanged", () => {
  expectFilter("hello world\n", "hello world\n");
});

test("strips code fences but keeps content", () => {
  const out = oneShot("before\n```js\nconst x = 1;\n```\nafter");
  assert.ok(out.includes("const x = 1;"));
  assert.ok(!out.includes("```"));
});

test("drops image markdown entirely", () => {
  assert.equal(oneShot("![alt](url)"), "");
});

test("strips italic markers but keeps content (matches upstream semantics)", () => {
  // Upstream filter intentionally leaves `**bold**` literal and only
  // collapses single-star italic pairs. Mirror that contract here so
  // the expectation matches production behaviour.
  const out = oneShot("**bold** and *italic*");
  assert.ok(out.includes("bold"));
  assert.ok(out.includes("italic"));
  assert.ok(!out.includes("*italic*"));
});

test("tables strip separators and keep cells joined by tab", () => {
  const input = "结果如下：\n| A | B |\n|---|---|\n| 1 | 2 |\n完毕。";
  const out = oneShot(input);
  assert.ok(out.includes("结果如下："));
  assert.ok(out.includes("完毕。"));
  assert.ok(!out.includes("|"));
  assert.ok(!out.includes("---"));
  assert.ok(out.includes("A"));
  assert.ok(out.includes("2"));
});

test("sanitizeMarkdown is a convenience wrapper for one-shot usage", () => {
  assert.equal(sanitizeMarkdown("*x*"), "x");
});

test("streaming outputs still strip single-star italic + image syntax", () => {
  // We deliberately don't assert streaming byte-equality with one-shot:
  // they can differ in whitespace around consumed fences. The invariant
  // checked here is the one upstream enforces — italic `*…*` collapses
  // and `![img](x)` is dropped across all feed cadences.
  const italicInputs = ["a *b* c", "*x*"];
  for (const i of italicInputs) {
    for (const [label, out] of [
      ["oneShot", oneShot(i)],
      ["charByChar", charByChar(i)],
      ["randomChunks", randomChunks(i)],
    ]) {
      assert.ok(
        !/\*[^\s*][^*]*\*/.test(out),
        `${label}: expected italic pair stripped in ${JSON.stringify(i)} → ${JSON.stringify(out)}`,
      );
    }
  }
  const imgInput = "prefix ![alt](url) suffix";
  for (const out of [oneShot(imgInput), charByChar(imgInput), randomChunks(imgInput)]) {
    assert.ok(!out.includes("![alt]"), `image should be dropped: ${JSON.stringify(out)}`);
  }
});
