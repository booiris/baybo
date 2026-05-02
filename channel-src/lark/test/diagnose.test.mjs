import test from "node:test";
import assert from "node:assert/strict";

import { LarkPlatform } from "../dist/platform.js";
import { LarkApprovals } from "../dist/approvals.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

// Minimal fake LarkChannel that LarkPlatform.bots needs for the
// diagnose path: just `botIdentity`. The platform's `bots` map is
// private; reach into it via the shape startBot would have produced
// by using a tiny test seam that mimics startBot's storage.
function withBotState(platform, botId, handle, config) {
  // eslint-disable-next-line no-underscore-dangle
  platform.bots.set(botId, { handle, config });
}

test("onDiagnoseRequested: unknown bot returns single error check", async () => {
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  const checks = await platform.onAgentDiagnoseRequested({ botId: "missing" });
  assert.equal(checks.length, 1);
  assert.equal(checks[0].name, "bot_state");
  assert.equal(checks[0].status, "error");
  assert.match(checks[0].detail, /not currently running/);
});

test("onDiagnoseRequested: known bot returns the full Phase 3.5 check surface", async () => {
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  withBotState(
    platform,
    "cli_a1",
    { botIdentity: { name: "AuraBot", openId: "ou_bot" } },
    { streaming: true, reactionEcho: false },
  );

  const checks = await platform.onAgentDiagnoseRequested({ botId: "cli_a1" });
  const names = checks.map((c) => c.name).sort();
  // Phase 3.5: bot_identity + transport + config + mcp_tools +
  // uat_pipeline + oauth_endpoint. uat_pipeline + oauth_endpoint
  // both `warn` since the test fixture has no UAT pipeline.
  assert.deepEqual(names, [
    "bot_identity",
    "config",
    "mcp_tools",
    "oauth_endpoint",
    "transport",
    "uat_pipeline",
  ]);

  const identity = checks.find((c) => c.name === "bot_identity");
  assert.equal(identity.status, "ok");
  assert.match(identity.detail, /name=AuraBot/);
  assert.match(identity.detail, /open_id=ou_bot/);

  const config = checks.find((c) => c.name === "config");
  assert.match(config.detail, /streaming=true/);
  assert.match(config.detail, /reaction_echo=false/);

  // mcp_tools count is whatever the server registers — non-zero
  // proves the harvest hook fired during the first session open.
  const tools = checks.find((c) => c.name === "mcp_tools");
  assert.equal(tools.status, "ok");
  assert.match(tools.detail, /\d+ MCP tools registered/);

  // No UAT pipeline on the bare test fixture — both UAT-related
  // checks downgrade to `warn` rather than throwing or surfacing
  // misleading errors.
  const uat = checks.find((c) => c.name === "uat_pipeline");
  assert.equal(uat.status, "warn");
  assert.match(uat.detail, /OAuth pipeline disabled/);

  const oauth = checks.find((c) => c.name === "oauth_endpoint");
  assert.equal(oauth.status, "warn");
  assert.match(oauth.detail, /OAuth pipeline disabled/);
});

test("onDiagnoseRequested: missing botIdentity downgrades to warn", async () => {
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  withBotState(
    platform,
    "cli_a1",
    { botIdentity: undefined },
    { streaming: false, reactionEcho: true },
  );

  const checks = await platform.onAgentDiagnoseRequested({ botId: "cli_a1" });
  const identity = checks.find((c) => c.name === "bot_identity");
  assert.equal(identity.status, "warn");
});

test("onDiagnoseRequested: uat_pipeline + oauth_endpoint surface live state when wired", async () => {
  // When the bot has an actual UAT pipeline, uat_pipeline reports
  // the user count + oauth_endpoint reaches the OAuth host. We
  // simulate the pipeline shape rather than spinning the full SDK
  // stack — the diagnose hook only reads `state.uat.store` +
  // `state.uat.accessor.baseUrl`.
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  let listUsersCalls = 0;
  const fakeUat = {
    accessor: { baseUrl: "https://open.feishu.cn" },
    scheduler: { stop: async () => undefined },
    authFlow: { cancelAll: () => undefined },
    store: {
      async listUsers() {
        listUsersCalls++;
        return ["ou_alice", "ou_bob"];
      },
    },
  };

  // eslint-disable-next-line no-underscore-dangle
  platform.bots.set("cli_b1", {
    handle: { botIdentity: { name: "BotB", openId: "ou_b" } },
    config: { streaming: true, reactionEcho: false },
    uat: fakeUat,
  });

  // Stub global fetch so the oauth_endpoint probe doesn't actually
  // hit the network during the test.
  const originalFetch = globalThis.fetch;
  let fetchCalls = 0;
  globalThis.fetch = async (url, _init) => {
    fetchCalls++;
    return { ok: false, status: 401 };
  };
  try {
    const checks = await platform.onAgentDiagnoseRequested({ botId: "cli_b1" });

    const uat = checks.find((c) => c.name === "uat_pipeline");
    assert.equal(uat.status, "ok");
    assert.match(uat.detail, /2 authorized users stored/);
    assert.equal(listUsersCalls, 1);

    const oauth = checks.find((c) => c.name === "oauth_endpoint");
    // 401 is still a successful round-trip — we just want to know
    // the host is reachable.
    assert.equal(oauth.status, "ok");
    assert.match(oauth.detail, /open\.feishu\.cn reachable \(HTTP 401\)/);
    assert.equal(fetchCalls, 1);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("onDiagnoseRequested: oauth_endpoint flags reachability failures", async () => {
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  const fakeUat = {
    accessor: { baseUrl: "https://open.feishu.cn" },
    scheduler: { stop: async () => undefined },
    authFlow: { cancelAll: () => undefined },
    store: { async listUsers() { return []; } },
  };
  // eslint-disable-next-line no-underscore-dangle
  platform.bots.set("cli_c1", {
    handle: { botIdentity: { name: "BotC", openId: "ou_c" } },
    config: { streaming: true, reactionEcho: false },
    uat: fakeUat,
  });

  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new Error("ECONNREFUSED");
  };
  try {
    const checks = await platform.onAgentDiagnoseRequested({ botId: "cli_c1" });
    const oauth = checks.find((c) => c.name === "oauth_endpoint");
    assert.equal(oauth.status, "error");
    assert.match(oauth.detail, /failed to reach/);
    assert.match(oauth.detail, /ECONNREFUSED/);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
