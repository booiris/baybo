import type { WeixinConfigManager } from "./api/config-cache.js";
import type { StreamingMarkdownFilter } from "./messaging/markdown-filter.js";

/**
 * Schema: JSON value written by the CLI login flow into the Aura secret
 * vault under `channel.weixin.bot.<bot_id>.token`. The sidecar reads it
 * verbatim from `StartBotCommand.token` and runs everything off the
 * fields here. The vault never persists a bare string, so the sidecar
 * parses strictly and refuses unversioned input.
 */
export interface AuthBlob {
  version: 1;
  botToken: string;
  baseUrl: string;
  userId: string;
  accountId: string;
  createdAt: string;
}

/**
 * Conversation address cached in BotChannel's routing tables. iLink
 * DMs have a single recipient id, so this is flat by design.
 */
export interface WeixinChat {
  toUserId: string;
}

/**
 * Opaque runtime handle passed back into the platform methods. The
 * `abort` controller stops the poll loop; `state` is shared mutable
 * state for the poll loop + outbound methods so redirected `baseUrl`
 * updates are visible everywhere.
 */
export interface WeixinBotHandle {
  readonly accountId: string;
  readonly state: RuntimeState;
}

export interface RuntimeState {
  readonly accountId: string;
  readonly botToken: string;
  baseUrl: string;
  readonly userId: string;
  getUpdatesBuf: string;
  readonly configMgr: WeixinConfigManager;
  readonly mdFilter: StreamingMarkdownFilter;
  readonly abort: AbortController;
  /**
   * Per-recipient `context_token`, updated from every inbound message.
   * iLink expects the most-recent token to be echoed on any reply —
   * lose it and replies fail with "invalid context". Keyed by the
   * peer's `@im.wechat` user id.
   */
  readonly contextTokens: Map<string, string>;
}
