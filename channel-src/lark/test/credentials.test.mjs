import test from "node:test";
import assert from "node:assert/strict";

import { parseStartBotCredentials } from "../dist/auth/credentials.js";
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
