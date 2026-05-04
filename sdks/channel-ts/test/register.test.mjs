import { spawn } from "node:child_process";
import readline from "node:readline";
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const DIST_REGISTER = fileURLToPath(
  new URL("../dist/register.js", import.meta.url),
);

function spawnRegistration(fnSrc) {
  const src = `
    import { runRegistration } from ${JSON.stringify(DIST_REGISTER)};
    await runRegistration(${fnSrc});
  `;
  return spawn(
    process.execPath,
    ["--input-type=module", "--eval", src],
    { stdio: ["pipe", "pipe", "inherit"] },
  );
}

async function collect(child) {
  const rl = readline.createInterface({ input: child.stdout });
  const frames = [];
  rl.on("line", (line) => {
    const msg = JSON.parse(line);
    frames.push(msg);
    if (msg.type === "prompt") {
      child.stdin.write(
        JSON.stringify({
          type: "prompt_reply",
          id: msg.id,
          value: `reply:${msg.label}`,
        }) + "\n",
      );
    }
  });
  const exitCode = await new Promise((resolve) => child.on("close", resolve));
  return { frames, exitCode };
}

test("runRegistration drives prompt cycle and emits result", async () => {
  const child = spawnRegistration(`async (ctx) => {
    const botId = await ctx.input("bot id");
    const token = await ctx.password("token");
    return { botId, token };
  }`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 0);
  assert.equal(frames.length, 3);
  assert.equal(frames[0].type, "prompt");
  assert.equal(frames[0].kind, "input");
  assert.equal(frames[0].label, "bot id");
  assert.equal(frames[1].type, "prompt");
  assert.equal(frames[1].kind, "password");
  assert.deepEqual(frames[2], {
    type: "result",
    bot_id: "reply:bot id",
    token: "reply:token",
  });
});

test("runRegistration thrown error emits error frame and exits 1", async () => {
  const child = spawnRegistration(`async () => { throw new Error("nope"); }`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 1);
  assert.equal(frames.length, 1);
  assert.equal(frames[0].type, "error");
  assert.ok(frames[0].message.includes("nope"));
});

test("runRegistration bad return value surfaces as error", async () => {
  const child = spawnRegistration(`async () => ({ nope: true })`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 1);
  assert.equal(frames.at(-1).type, "error");
  assert.ok(frames.at(-1).message.includes("botId"));
});

test("runRegistration round-trips metadata + secrets through the result frame", async () => {
  // Codex review regression: the metadata path was dead code because
  // RegistrationResult only carried botId/token. The CLI must receive
  // both maps verbatim, with empty maps stripped to keep older CLIs
  // round-tripping.
  const child = spawnRegistration(`async () => ({
    botId: "lark-bot",
    token: "primary",
    metadata: { base_url: "https://open.feishu.cn" },
    secrets: { app_secret: "shh-app", encrypt_key: "shh-encrypt" },
  })`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 0);
  const result = frames.at(-1);
  assert.equal(result.type, "result");
  assert.equal(result.bot_id, "lark-bot");
  assert.equal(result.token, "primary");
  assert.deepEqual(result.metadata, { base_url: "https://open.feishu.cn" });
  assert.deepEqual(result.secrets, {
    app_secret: "shh-app",
    encrypt_key: "shh-encrypt",
  });
});

test("runRegistration omits empty metadata/secrets to stay backward-compatible", async () => {
  const child = spawnRegistration(`async () => ({
    botId: "tg-bot",
    token: "tok",
  })`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 0);
  const result = frames.at(-1);
  assert.deepEqual(result, {
    type: "result",
    bot_id: "tg-bot",
    token: "tok",
  });
  assert.equal(result.metadata, undefined);
  assert.equal(result.secrets, undefined);
});

test("runRegistration rejects non-string values inside secrets", async () => {
  const child = spawnRegistration(`async () => ({
    botId: "b",
    token: "t",
    secrets: { app_secret: 123 },
  })`);
  const { frames, exitCode } = await collect(child);
  assert.equal(exitCode, 1);
  const last = frames.at(-1);
  assert.equal(last.type, "error");
  assert.ok(
    last.message.includes("secrets.app_secret"),
    `expected error to name the bad key, got: ${last.message}`,
  );
});
