#!/usr/bin/env node
import { runSidecar } from "@aura/channel-sdk";
import { BotChannel } from "@aura/channel-sdk/bot";

import { WeixinApprovals } from "./approvals.js";
import { runLogin } from "./cli.js";
import { WeixinPlatform } from "./platform.js";
import type { WeixinBotHandle, WeixinChat } from "./types.js";

void runSidecar({
  channelType: "weixin",
  build: (logger) => {
    const approvals = new WeixinApprovals(logger);
    return new BotChannel<WeixinBotHandle, WeixinChat>({
      channelType: "weixin",
      logger,
      platform: new WeixinPlatform(logger, approvals),
      approvals,
    });
  },
  register: async (_ctx) => runLogin(),
});
