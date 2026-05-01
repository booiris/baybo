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

export interface BotRuntimeConfig {
  /** Stream agent deltas through CardKit v2; default on. */
  streaming: boolean;
  /** Post a 👍 reaction on every accepted inbound; default off. */
  reactionEcho: boolean;
}

const DEFAULT_BASE_URL = "https://open.feishu.cn";

const DEFAULT_BOT_RUNTIME_CONFIG: BotRuntimeConfig = {
  streaming: true,
  reactionEcho: false,
};

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

/**
 * Extract per-bot runtime knobs from `StartBot.metadata`. Operator-set
 * values flow through plaintext metadata (not the secret vault); empty /
 * unrecognised values fall back to the defaults so an old aura sidecar
 * keeps working when a new key is introduced. Booleans accept the
 * standard idioms (`on/off`, `true/false`, `1/0`, `yes/no`) with leading
 * / trailing whitespace tolerated.
 */
export function parseBotRuntimeConfig(
  cmd: StartBotCommand,
): BotRuntimeConfig {
  return {
    streaming: parseBool(
      cmd.metadata["streaming"],
      DEFAULT_BOT_RUNTIME_CONFIG.streaming,
    ),
    reactionEcho: parseBool(
      cmd.metadata["reaction_echo"],
      DEFAULT_BOT_RUNTIME_CONFIG.reactionEcho,
    ),
  };
}

function parseBool(value: string | undefined, fallback: boolean): boolean {
  if (typeof value !== "string") return fallback;
  const v = value.trim().toLowerCase();
  if (v === "") return fallback;
  if (v === "on" || v === "true" || v === "1" || v === "yes") return true;
  if (v === "off" || v === "false" || v === "0" || v === "no") return false;
  return fallback;
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
