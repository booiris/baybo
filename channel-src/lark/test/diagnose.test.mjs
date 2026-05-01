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

  const checks = await platform.onDiagnoseRequested({ botId: "missing" });
  assert.equal(checks.length, 1);
  assert.equal(checks[0].name, "bot_state");
  assert.equal(checks[0].status, "error");
  assert.match(checks[0].detail, /not currently running/);
});

test("onDiagnoseRequested: known bot returns identity + transport + config rows", async () => {
  const approvals = new LarkApprovals(noopLogger);
  const platform = new LarkPlatform(noopLogger, approvals);

  withBotState(
    platform,
    "cli_a1",
    { botIdentity: { name: "AuraBot", openId: "ou_bot" } },
    { streaming: true, reactionEcho: false },
  );

  const checks = await platform.onDiagnoseRequested({ botId: "cli_a1" });
  const names = checks.map((c) => c.name).sort();
  assert.deepEqual(names, ["bot_identity", "config", "transport"]);

  const identity = checks.find((c) => c.name === "bot_identity");
  assert.equal(identity.status, "ok");
  assert.match(identity.detail, /name=AuraBot/);
  assert.match(identity.detail, /open_id=ou_bot/);

  const config = checks.find((c) => c.name === "config");
  assert.match(config.detail, /streaming=true/);
  assert.match(config.detail, /reaction_echo=false/);
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

  const checks = await platform.onDiagnoseRequested({ botId: "cli_a1" });
  const identity = checks.find((c) => c.name === "bot_identity");
  assert.equal(identity.status, "warn");
});
