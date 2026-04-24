/**
 * Standalone CLI entry for the weixin QR login flow.
 *
 * Spawned by `aura channel add` (see `crates/cli/src/commands/channel/
 * weixin_login.rs`) with `AURA_WEIXIN_MODE=login`. Renders the QR to
 * stdout so the operator can scan, then prints a single machine-parsable
 * marker line when the login completes:
 *
 *     AURA_WEIXIN_LOGIN_RESULT:<json>\n
 *
 * The Rust parent parses that line, stores the blob in the vault, and
 * registers the bot. All human-facing output (QR ASCII, progress) goes
 * to stderr so stdout is clean for the marker.
 */
import qrcode from "qrcode-terminal";

import {
  DEFAULT_ILINK_BOT_TYPE,
  FIXED_BASE_URL,
  startWeixinLoginWithQr,
  waitForWeixinLogin,
} from "./auth/login-qr.js";
import { normalizeBotId } from "./auth/normalize.js";
import type { AuthBlob } from "./types.js";

const LOGIN_RESULT_MARKER = "AURA_WEIXIN_LOGIN_RESULT:";

function out(msg: string): void {
  process.stderr.write(msg);
}

async function renderQr(qrcodeContent: string): Promise<void> {
  await new Promise<void>((resolve) => {
    qrcode.generate(qrcodeContent, { small: true }, (qr: string) => {
      process.stderr.write(qr + "\n");
      resolve();
    });
  });
  out(`如果二维码未能成功展示，请用浏览器打开以下链接扫码：\n`);
  out(`${qrcodeContent}\n\n`);
}

export async function runLogin(): Promise<void> {
  const timeoutMs = Number(process.env.AURA_WEIXIN_LOGIN_TIMEOUT_MS ?? 480_000);
  const apiBaseUrl = process.env.AURA_WEIXIN_API_BASE_URL || FIXED_BASE_URL;
  const botType = process.env.AURA_WEIXIN_BOT_TYPE || DEFAULT_ILINK_BOT_TYPE;

  const controller = new AbortController();
  const onSignal = () => {
    out("\n取消登录。\n");
    controller.abort();
    process.exit(130);
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  out("正在启动微信扫码登录...\n");
  const start = await startWeixinLoginWithQr({
    apiBaseUrl,
    botType,
    force: true,
  });
  if (!start.qrcodeUrl) {
    out(`${start.message}\n`);
    process.exit(1);
  }

  out("\n使用微信扫描以下二维码，以完成连接：\n\n");
  await renderQr(start.qrcodeUrl);

  out("等待连接结果...\n");
  const result = await waitForWeixinLogin({
    sessionKey: start.sessionKey,
    timeoutMs,
    apiBaseUrl,
    botType,
    signal: controller.signal,
    onStatus: (status) => {
      if (status === "scaned") out("👀 已扫码，在微信继续操作...\n");
    },
    onRefreshed: (newUrl, attempt, max) => {
      void (async () => {
        out(`\n⏳ 二维码已过期，正在刷新...(${attempt}/${max})\n\n`);
        await renderQr(newUrl);
      })();
    },
  });

  if (!result.connected) {
    out(`${result.message}\n`);
    process.exit(1);
  }

  if (!result.accountId) {
    out(`登录失败：服务器未返回 accountId。\n`);
    process.exit(1);
  }
  if (!result.botToken) {
    out(`登录失败：服务器未返回 botToken。\n`);
    process.exit(1);
  }

  const blob: AuthBlob = {
    version: 1,
    botToken: result.botToken,
    baseUrl: result.baseUrl || apiBaseUrl,
    userId: result.userId || "",
    accountId: normalizeBotId(result.accountId),
    createdAt: new Date().toISOString(),
  };

  out(`\n${result.message}\n`);
  process.stdout.write(`${LOGIN_RESULT_MARKER}${JSON.stringify(blob)}\n`);
  process.exit(0);
}
