import test from "node:test";
import assert from "node:assert/strict";

import { markdownToTelegram, TELEGRAM_PARSE_MODE } from "../dist/markdown.js";

test("parse mode constant is MarkdownV2", () => {
  assert.equal(TELEGRAM_PARSE_MODE, "MarkdownV2");
});

test("empty / whitespace input collapses to empty", () => {
  assert.equal(markdownToTelegram(""), "");
  assert.equal(markdownToTelegram(" "), "");
  assert.equal(markdownToTelegram("\n"), "");
});

test("plain text round-trips with no trailing newline", () => {
  assert.equal(markdownToTelegram("look at this"), "look at this");
  assert.equal(markdownToTelegram("hello\n"), "hello");
});

test("dots in version numbers get backslash-escaped", () => {
  assert.equal(markdownToTelegram("v1.0.0"), "v1\\.0\\.0");
});

test("**bold** / _italic_ / `code` map to MarkdownV2 markers", () => {
  assert.equal(
    markdownToTelegram("**bold** _italic_ `code`"),
    "*bold* _italic_ `code`",
  );
});

test("# heading renders as bold", () => {
  assert.equal(markdownToTelegram("# Title"), "*Title*");
});

test("links pass through with their URL preserved", () => {
  assert.equal(
    markdownToTelegram("[home](https://example.com)"),
    "[home](https://example.com)",
  );
});

test("unordered list lines render with bullet glyph", () => {
  const out = markdownToTelegram("- one\n- two");
  assert.match(out, /•\s+one/);
  assert.match(out, /•\s+two/);
});
