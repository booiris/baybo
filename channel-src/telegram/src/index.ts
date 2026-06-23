#!/usr/bin/env node
import { runSidecar } from "@baybo/channel-sdk";
import { BotChannel } from "@baybo/channel-sdk/bot";
import type { Bot } from "grammy";

import { TelegramApprovals } from "./approvals.js";
import { TelegramPlatform, type TelegramChat } from "./platform.js";

void runSidecar({
  channelType: "telegram",
  build: (logger) =>
    new BotChannel<Bot, TelegramChat>({
      channelType: "telegram",
      logger,
      platform: new TelegramPlatform(logger),
      approvals: new TelegramApprovals(logger),
      // Slash commands flow in via Frame::SlashManifest from the
      // gateway (single source of truth in
      // `crates/gateway/src/channel/slash.rs::manifest`). No local
      // list is maintained.
    }),
  register: async (ctx) => {
    const token = await ctx.password("bot token: ", { required: true });
    const colon = token.indexOf(":");
    if (colon <= 0) {
      throw new Error(
        "telegram bot tokens must look like `<numeric_id>:<secret>`",
      );
    }
    const prefix = token.slice(0, colon);
    const suffix = token.slice(colon + 1);
    if (!/^\d+$/.test(prefix)) {
      throw new Error(
        "telegram bot id (the part before `:`) must be a non-empty numeric string",
      );
    }
    if (!suffix) {
      throw new Error(
        "telegram bot token (the part after `:`) must not be empty",
      );
    }
    return { botId: prefix, token };
  },
});
