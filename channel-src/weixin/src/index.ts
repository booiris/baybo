#!/usr/bin/env node
/*!
 * Baybo weixin channel sidecar.
 *
 * Derived from openclaw-weixin (https://github.com/Tencent/openclaw-weixin),
 * Copyright (C) 2026 Tencent, licensed under the MIT License. The iLink
 * protocol layer (login QR flow, polling cadence, markdown filter, message
 * encoding) is ported from that project; the surrounding sidecar runtime,
 * approvals, and integration with the Baybo gateway are original.
 */
import { runSidecar } from "@baybo/channel-sdk";
import { BotChannel } from "@baybo/channel-sdk/bot";

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
