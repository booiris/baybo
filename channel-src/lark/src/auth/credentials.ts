import * as lark from "@larksuiteoapi/node-sdk";

import type { StartBotCommand } from "@aura/channel-sdk";

export interface AppCredentials {
  appId: string;
  appSecret: string;
  // The Lark SDK accepts either a `Domain` enum value or a free-form
  // base URL string for `client.domain` — see the report §4.0.1
  // "free-form base_url stays string, not enum" decision. Three
  // mainstream hosts get an enum mapping for nicer logging; everything
  // else flows through verbatim.
  domain: lark.Domain | string;
}

const DEFAULT_BASE_URL = "https://open.feishu.cn";

export function parseStartBotCredentials(cmd: StartBotCommand): AppCredentials {
  const appId = cmd.token.trim();
  if (!appId) {
    throw new Error("lark StartBot: token (app_id) is empty");
  }
  const appSecret = cmd.metadata["app_secret"];
  if (typeof appSecret !== "string" || appSecret.length === 0) {
    throw new Error(
      "lark StartBot: metadata.app_secret missing — register the bot with `aura channel add lark`",
    );
  }
  const baseUrl = (cmd.metadata["base_url"] ?? DEFAULT_BASE_URL).trim();
  return { appId, appSecret, domain: domainFromBaseUrl(baseUrl) };
}

// Map the three known Lark deployment hosts onto the SDK's typed
// Domain enum so logs and the rare typed code path render the human
// name; anything else (private cloud, future regional mirror) passes
// through as the raw string the SDK will plug into its endpoints.
function domainFromBaseUrl(baseUrl: string): lark.Domain | string {
  let host: string;
  try {
    host = new URL(baseUrl).host.toLowerCase();
  } catch {
    return baseUrl;
  }
  if (host === "open.feishu.cn") return lark.Domain.Feishu;
  if (host === "open.larksuite.com") return lark.Domain.Lark;
  return baseUrl;
}
