import test from "node:test";
import assert from "node:assert/strict";

import { cleanInboundContent } from "../dist/messaging/inbound.js";

test("cleanInboundContent: empty mentions trims whitespace, leaves text intact", () => {
  assert.equal(cleanInboundContent("  hello world  ", []), "hello world");
});

test("cleanInboundContent: strips bot mention placeholder", () => {
  const content = "@_user_1 what's the weather";
  const mentions = [
    { key: "@_user_1", openId: "ou_bot", name: "AuraBot", isBot: true },
  ];
  assert.equal(cleanInboundContent(content, mentions), "what's the weather");
});

test("cleanInboundContent: strips multiple mentions including @all", () => {
  const content = "@_all hello @_user_1 @_user_2 please review";
  const mentions = [
    { key: "@_all", isBot: false },
    { key: "@_user_1", openId: "ou_a", name: "Alice", isBot: false },
    { key: "@_user_2", openId: "ou_b", name: "Bob", isBot: false },
  ];
  assert.equal(
    cleanInboundContent(content, mentions),
    "hello please review",
  );
});

test("cleanInboundContent: collapses repeated whitespace into single spaces", () => {
  const content = "@_user_1   ping  \n  @_user_2";
  const mentions = [
    { key: "@_user_1", isBot: true },
    { key: "@_user_2", isBot: false },
  ];
  assert.equal(cleanInboundContent(content, mentions), "ping");
});

test("cleanInboundContent: leaves text untouched when no placeholder matches", () => {
  const content = "no mentions here";
  const mentions = [{ key: "@_user_99", isBot: false }];
  assert.equal(cleanInboundContent(content, mentions), "no mentions here");
});

test("cleanInboundContent: skips mentions with empty key", () => {
  const content = "raw text";
  const mentions = [{ key: "", isBot: false }];
  assert.equal(cleanInboundContent(content, mentions), "raw text");
});
