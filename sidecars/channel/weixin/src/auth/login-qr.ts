import { randomUUID } from "node:crypto";

import { apiGetFetch } from "../api/http.js";

type Status = "wait" | "scaned" | "confirmed" | "expired" | "scaned_but_redirect";

type ActiveLogin = {
  sessionKey: string;
  qrcode: string;
  qrcodeUrl: string;
  startedAt: number;
  currentApiBaseUrl?: string;
};

const ACTIVE_LOGIN_TTL_MS = 5 * 60_000;
const QR_LONG_POLL_TIMEOUT_MS = 35_000;

export const DEFAULT_ILINK_BOT_TYPE = "3";
export const FIXED_BASE_URL = "https://ilinkai.weixin.qq.com";
export const DEFAULT_CDN_BASE_URL = "https://novac2c.cdn.weixin.qq.com/c2c";

const activeLogins = new Map<string, ActiveLogin>();

interface QRCodeResponse {
  qrcode: string;
  qrcode_img_content: string;
}

interface StatusResponse {
  status: Status;
  bot_token?: string;
  ilink_bot_id?: string;
  baseurl?: string;
  ilink_user_id?: string;
  redirect_host?: string;
}

function isLoginFresh(login: ActiveLogin): boolean {
  return Date.now() - login.startedAt < ACTIVE_LOGIN_TTL_MS;
}

function purgeExpired(): void {
  for (const [id, login] of activeLogins) {
    if (!isLoginFresh(login)) activeLogins.delete(id);
  }
}

async function fetchQRCode(apiBaseUrl: string, botType: string): Promise<QRCodeResponse> {
  const rawText = await apiGetFetch({
    baseUrl: apiBaseUrl,
    endpoint: `ilink/bot/get_bot_qrcode?bot_type=${encodeURIComponent(botType)}`,
    label: "fetchQRCode",
  });
  return JSON.parse(rawText) as QRCodeResponse;
}

async function pollQRStatus(apiBaseUrl: string, qrcode: string): Promise<StatusResponse> {
  try {
    const rawText = await apiGetFetch({
      baseUrl: apiBaseUrl,
      endpoint: `ilink/bot/get_qrcode_status?qrcode=${encodeURIComponent(qrcode)}`,
      timeoutMs: QR_LONG_POLL_TIMEOUT_MS,
      label: "pollQRStatus",
    });
    return JSON.parse(rawText) as StatusResponse;
  } catch (err) {
    if (err instanceof Error && err.name === "AbortError") {
      return { status: "wait" };
    }
    return { status: "wait" };
  }
}

export type WeixinQrStartResult = {
  qrcodeUrl?: string;
  message: string;
  sessionKey: string;
};

export type WeixinQrWaitResult = {
  connected: boolean;
  botToken?: string;
  accountId?: string;
  baseUrl?: string;
  userId?: string;
  message: string;
};

export async function startWeixinLoginWithQr(opts: {
  force?: boolean;
  accountId?: string;
  apiBaseUrl?: string;
  botType?: string;
}): Promise<WeixinQrStartResult> {
  const sessionKey = opts.accountId || randomUUID();
  purgeExpired();

  const existing = activeLogins.get(sessionKey);
  if (!opts.force && existing && isLoginFresh(existing) && existing.qrcodeUrl) {
    return {
      qrcodeUrl: existing.qrcodeUrl,
      message: "二维码已就绪，请使用微信扫描。",
      sessionKey,
    };
  }

  try {
    const botType = opts.botType || DEFAULT_ILINK_BOT_TYPE;
    const qr = await fetchQRCode(opts.apiBaseUrl || FIXED_BASE_URL, botType);
    activeLogins.set(sessionKey, {
      sessionKey,
      qrcode: qr.qrcode,
      qrcodeUrl: qr.qrcode_img_content,
      startedAt: Date.now(),
    });
    return {
      qrcodeUrl: qr.qrcode_img_content,
      message: "使用微信扫描以下二维码，以完成连接。",
      sessionKey,
    };
  } catch (err) {
    return {
      message: `Failed to start login: ${String(err)}`,
      sessionKey,
    };
  }
}

const MAX_QR_REFRESH_COUNT = 3;

export async function waitForWeixinLogin(opts: {
  sessionKey: string;
  timeoutMs?: number;
  apiBaseUrl?: string;
  botType?: string;
  onStatus?: (status: Status) => void;
  onRefreshed?: (qrcodeUrl: string, attempt: number, maxAttempts: number) => void;
  signal?: AbortSignal;
}): Promise<WeixinQrWaitResult> {
  const activeLogin = activeLogins.get(opts.sessionKey);
  if (!activeLogin) {
    return { connected: false, message: "当前没有进行中的登录，请先发起登录。" };
  }
  if (!isLoginFresh(activeLogin)) {
    activeLogins.delete(opts.sessionKey);
    return { connected: false, message: "二维码已过期，请重新生成。" };
  }

  const timeoutMs = Math.max(opts.timeoutMs ?? 480_000, 1000);
  const deadline = Date.now() + timeoutMs;
  let qrRefreshCount = 1;

  activeLogin.currentApiBaseUrl = opts.apiBaseUrl || FIXED_BASE_URL;

  while (Date.now() < deadline) {
    if (opts.signal?.aborted) {
      activeLogins.delete(opts.sessionKey);
      return { connected: false, message: "登录已取消。" };
    }
    try {
      const currentBaseUrl = activeLogin.currentApiBaseUrl ?? FIXED_BASE_URL;
      const resp = await pollQRStatus(currentBaseUrl, activeLogin.qrcode);
      opts.onStatus?.(resp.status);
      switch (resp.status) {
        case "wait":
        case "scaned":
          break;
        case "expired": {
          qrRefreshCount++;
          if (qrRefreshCount > MAX_QR_REFRESH_COUNT) {
            activeLogins.delete(opts.sessionKey);
            return {
              connected: false,
              message: `登录超时：二维码多次过期，请重新开始登录流程。`,
            };
          }
          try {
            const botType = opts.botType || DEFAULT_ILINK_BOT_TYPE;
            const qr = await fetchQRCode(opts.apiBaseUrl || FIXED_BASE_URL, botType);
            activeLogin.qrcode = qr.qrcode;
            activeLogin.qrcodeUrl = qr.qrcode_img_content;
            activeLogin.startedAt = Date.now();
            opts.onRefreshed?.(qr.qrcode_img_content, qrRefreshCount, MAX_QR_REFRESH_COUNT);
          } catch (refreshErr) {
            activeLogins.delete(opts.sessionKey);
            return {
              connected: false,
              message: `刷新二维码失败: ${String(refreshErr)}`,
            };
          }
          break;
        }
        case "scaned_but_redirect": {
          if (resp.redirect_host) {
            activeLogin.currentApiBaseUrl = `https://${resp.redirect_host}`;
          }
          break;
        }
        case "confirmed": {
          activeLogins.delete(opts.sessionKey);
          if (!resp.ilink_bot_id) {
            return {
              connected: false,
              message: "登录失败：服务器未返回 ilink_bot_id。",
            };
          }
          return {
            connected: true,
            ...(resp.bot_token !== undefined ? { botToken: resp.bot_token } : {}),
            accountId: resp.ilink_bot_id,
            ...(resp.baseurl !== undefined ? { baseUrl: resp.baseurl } : {}),
            ...(resp.ilink_user_id !== undefined ? { userId: resp.ilink_user_id } : {}),
            message: "✅ 与微信连接成功！",
          };
        }
      }
    } catch (err) {
      activeLogins.delete(opts.sessionKey);
      return { connected: false, message: `Login failed: ${String(err)}` };
    }
    await new Promise<void>((r) => setTimeout(r, 1000));
  }

  activeLogins.delete(opts.sessionKey);
  return { connected: false, message: "登录超时，请重试。" };
}

/** @internal */
export function _resetForTest(): void {
  activeLogins.clear();
}
