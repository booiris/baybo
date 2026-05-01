import test from "node:test";
import assert from "node:assert/strict";

import {
  parseBotRuntimeConfig,
  parseStartBotCredentials,
} from "../dist/auth/credentials.js";
import * as lark from "@larksuiteoapi/node-sdk";

const baseCmd = (extraMetadata = {}) => ({
  botId: "cli_a1b2",
  token: "cli_a1b2",
  metadata: { app_secret: "s3cret", ...extraMetadata },
});

test("parseStartBotCredentials: happy path defaults to Feishu domain", () => {
  const c = parseStartBotCredentials(baseCmd());
  assert.equal(c.appId, "cli_a1b2");
  assert.equal(c.appSecret, "s3cret");
  assert.equal(c.domain, lark.Domain.Feishu);
});

test("parseStartBotCredentials: lark host maps to Domain.Lark", () => {
  const c = parseStartBotCredentials(
    baseCmd({ base_url: "https://open.larksuite.com" }),
  );
  assert.equal(c.domain, lark.Domain.Lark);
});

test("parseStartBotCredentials: unknown host passes through as string", () => {
  const c = parseStartBotCredentials(
    baseCmd({ base_url: "https://open.volc-feishu.cn" }),
  );
  assert.equal(c.domain, "https://open.volc-feishu.cn");
});

test("parseStartBotCredentials: malformed base_url passes through verbatim (the SDK validates)", () => {
  const c = parseStartBotCredentials(baseCmd({ base_url: "not a url" }));
  assert.equal(c.domain, "not a url");
});

test("parseStartBotCredentials: missing app_secret throws", () => {
  assert.throws(
    () =>
      parseStartBotCredentials({
        botId: "cli_a1",
        token: "cli_a1",
        metadata: {},
      }),
    /app_secret missing/,
  );
});

test("parseStartBotCredentials: empty token throws", () => {
  assert.throws(
    () =>
      parseStartBotCredentials({
        botId: "cli_a1",
        token: "",
        metadata: { app_secret: "x" },
      }),
    /app_id\) is empty/,
  );
});

test("parseBotRuntimeConfig: defaults are streaming on, reaction echo off", () => {
  const c = parseBotRuntimeConfig(baseCmd());
  assert.equal(c.streaming, true);
  assert.equal(c.reactionEcho, false);
});

test("parseBotRuntimeConfig: explicit overrides flip the defaults", () => {
  const c = parseBotRuntimeConfig(
    baseCmd({ streaming: "off", reaction_echo: "on" }),
  );
  assert.equal(c.streaming, false);
  assert.equal(c.reactionEcho, true);
});

test("parseBotRuntimeConfig: accepts true/false/yes/no/1/0 with whitespace", () => {
  for (const val of ["true", "yes", "1", " on ", "TRUE", "Yes"]) {
    assert.equal(
      parseBotRuntimeConfig(baseCmd({ reaction_echo: val })).reactionEcho,
      true,
      `expected ${JSON.stringify(val)} → true`,
    );
  }
  for (const val of ["false", "no", "0", " off ", "FALSE", "No"]) {
    assert.equal(
      parseBotRuntimeConfig(baseCmd({ streaming: val })).streaming,
      false,
      `expected ${JSON.stringify(val)} → false`,
    );
  }
});

test("parseBotRuntimeConfig: unrecognised values fall back to defaults", () => {
  const c = parseBotRuntimeConfig(
    baseCmd({ streaming: "maybe", reaction_echo: "" }),
  );
  // streaming default is on; bad value doesn't flip it
  assert.equal(c.streaming, true);
  // empty string is "fall back to default" (false)
  assert.equal(c.reactionEcho, false);
});
