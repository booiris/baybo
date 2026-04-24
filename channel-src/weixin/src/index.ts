#!/usr/bin/env node
import { runSidecar } from "@aura/channel-sdk";
import { BotChannel } from "@aura/channel-sdk/bot";

import { WeixinApprovals } from "./approvals.js";
import { WeixinPlatform } from "./platform.js";
import type { WeixinBotHandle, WeixinChat } from "./types.js";

if (process.env.AURA_WEIXIN_MODE === "login") {
  const mod = await import("./cli.js");
  await mod.runLogin();
} else {
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
  });
}
