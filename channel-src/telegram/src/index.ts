#!/usr/bin/env node
import { runSidecar } from "@aura/channel-sdk";
import { BotChannel } from "@aura/channel-sdk/bot";
import type { Bot } from "grammy";

import { TelegramApprovals } from "./approvals.js";
import { TelegramPlatform, type TelegramChat } from "./transport.js";

void runSidecar({
  channelType: "telegram",
  build: (logger) =>
    new BotChannel<Bot, TelegramChat>({
      channelType: "telegram",
      logger,
      platform: new TelegramPlatform(logger),
      approvals: new TelegramApprovals(logger),
    }),
});
