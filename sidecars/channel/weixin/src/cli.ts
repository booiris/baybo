import type { RegistrationResult } from "@baybo/channel-sdk";
import qrcode from "qrcode-terminal";

import {
  DEFAULT_CDN_BASE_URL,
  DEFAULT_ILINK_BOT_TYPE,
  FIXED_BASE_URL,
  startWeixinLoginWithQr,
  waitForWeixinLogin,
} from "./auth/login-qr.js";
import { normalizeBotId } from "./auth/normalize.js";
import type { AuthBlob } from "./types.js";

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

export async function runLogin(): Promise<RegistrationResult> {
  const timeoutMs = Number(process.env.BAYBO_WEIXIN_LOGIN_TIMEOUT_MS ?? 480_000);
  const apiBaseUrl = process.env.BAYBO_WEIXIN_API_BASE_URL || FIXED_BASE_URL;
  const botType = process.env.BAYBO_WEIXIN_BOT_TYPE || DEFAULT_ILINK_BOT_TYPE;

  out("正在启动微信扫码登录...\n");
  const start = await startWeixinLoginWithQr({
    apiBaseUrl,
    botType,
    force: true,
  });
  if (!start.qrcodeUrl) {
    throw new Error(start.message || "weixin login failed to produce a QR");
  }

  out("\n使用微信扫描以下二维码，以完成连接：\n\n");
  await renderQr(start.qrcodeUrl);

  out("等待连接结果...\n");
  const result = await waitForWeixinLogin({
    sessionKey: start.sessionKey,
    timeoutMs,
    apiBaseUrl,
    botType,
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
    throw new Error(result.message || "weixin login did not complete");
  }
  if (!result.accountId) {
    throw new Error("登录失败：服务器未返回 accountId。");
  }
  if (!result.botToken) {
    throw new Error("登录失败：服务器未返回 botToken。");
  }

  const cdnBaseUrl = process.env.BAYBO_WEIXIN_CDN_BASE_URL || DEFAULT_CDN_BASE_URL;
  const blob: AuthBlob = {
    version: 1,
    botToken: result.botToken,
    baseUrl: result.baseUrl || apiBaseUrl,
    cdnBaseUrl,
    userId: result.userId || "",
    accountId: normalizeBotId(result.accountId),
    createdAt: new Date().toISOString(),
  };

  out(`\n${result.message}\n`);
  return { botId: blob.accountId, token: JSON.stringify(blob) };
}
