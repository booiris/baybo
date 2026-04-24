import {
  DEFAULT_API_TIMEOUT_MS,
  DEFAULT_CONFIG_TIMEOUT_MS,
  DEFAULT_LONG_POLL_TIMEOUT_MS,
  apiPostFetch,
  buildBaseInfo,
} from "./http.js";
import type {
  GetConfigResp,
  GetUpdatesReq,
  GetUpdatesResp,
  SendMessageReq,
  SendTypingReq,
} from "./types.js";

export interface WeixinApiOptions {
  baseUrl: string;
  token?: string;
  timeoutMs?: number;
}

/**
 * Long-poll getUpdates. Client-side AbortError is treated as a normal
 * long-poll timeout and returns a 0-msg response with the buf unchanged.
 */
export async function getUpdates(
  params: GetUpdatesReq & WeixinApiOptions & { signal?: AbortSignal },
): Promise<GetUpdatesResp> {
  const timeout = params.timeoutMs ?? DEFAULT_LONG_POLL_TIMEOUT_MS;
  try {
    const rawText = await apiPostFetch({
      baseUrl: params.baseUrl,
      endpoint: "ilink/bot/getupdates",
      body: JSON.stringify({
        get_updates_buf: params.get_updates_buf ?? "",
        base_info: buildBaseInfo(),
      }),
      ...(params.token !== undefined ? { token: params.token } : {}),
      timeoutMs: timeout,
      label: "getUpdates",
      ...(params.signal !== undefined ? { signal: params.signal } : {}),
    });
    return JSON.parse(rawText) as GetUpdatesResp;
  } catch (err) {
    if (err instanceof Error && err.name === "AbortError") {
      return {
        ret: 0,
        msgs: [],
        ...(params.get_updates_buf !== undefined ? { get_updates_buf: params.get_updates_buf } : {}),
      };
    }
    throw err;
  }
}

export async function sendMessage(
  params: WeixinApiOptions & { body: SendMessageReq },
): Promise<void> {
  const rawText = await apiPostFetch({
    baseUrl: params.baseUrl,
    endpoint: "ilink/bot/sendmessage",
    body: JSON.stringify({ ...params.body, base_info: buildBaseInfo() }),
    ...(params.token !== undefined ? { token: params.token } : {}),
    timeoutMs: params.timeoutMs ?? DEFAULT_API_TIMEOUT_MS,
    label: "sendMessage",
  });
  // iLink can return HTTP 200 with an error body; don't let the 2xx
  // mask a business failure. A non-JSON 2xx (proxy error page,
  // wrong-host splash, expired-auth HTML) is also treated as failure —
  // silently succeeding there would drop the user's message without
  // any signal to operators, which was the v1 behaviour we're
  // correcting here.
  let parsed: { ret?: number; errcode?: number; errmsg?: string };
  try {
    parsed = JSON.parse(rawText) as { ret?: number; errcode?: number; errmsg?: string };
  } catch (e) {
    throw new Error(
      `sendMessage non-JSON 2xx body: ${String(e)} raw=${rawText.slice(0, 200)}`,
    );
  }
  const code = parsed.errcode ?? parsed.ret;
  if (code !== undefined && code !== 0) {
    throw new Error(
      `sendMessage errcode=${code} errmsg=${parsed.errmsg ?? ""} raw=${rawText.slice(0, 200)}`,
    );
  }
}

export async function getConfig(
  params: WeixinApiOptions & { ilinkUserId: string; contextToken?: string },
): Promise<GetConfigResp> {
  const rawText = await apiPostFetch({
    baseUrl: params.baseUrl,
    endpoint: "ilink/bot/getconfig",
    body: JSON.stringify({
      ilink_user_id: params.ilinkUserId,
      context_token: params.contextToken,
      base_info: buildBaseInfo(),
    }),
    ...(params.token !== undefined ? { token: params.token } : {}),
    timeoutMs: params.timeoutMs ?? DEFAULT_CONFIG_TIMEOUT_MS,
    label: "getConfig",
  });
  return JSON.parse(rawText) as GetConfigResp;
}

export async function sendTyping(
  params: WeixinApiOptions & { body: SendTypingReq },
): Promise<void> {
  await apiPostFetch({
    baseUrl: params.baseUrl,
    endpoint: "ilink/bot/sendtyping",
    body: JSON.stringify({ ...params.body, base_info: buildBaseInfo() }),
    ...(params.token !== undefined ? { token: params.token } : {}),
    timeoutMs: params.timeoutMs ?? DEFAULT_CONFIG_TIMEOUT_MS,
    label: "sendTyping",
  });
}
