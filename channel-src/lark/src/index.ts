#!/usr/bin/env node
import { runSidecar } from "@aura/channel-sdk";
import { BotChannel } from "@aura/channel-sdk/bot";
import type * as lark from "@larksuiteoapi/node-sdk";

import { LarkApprovals } from "./approvals.js";
import { runLarkRegistration } from "./auth/register-flow.js";
import { CHANNEL_TYPE, LarkPlatform, type LarkChat } from "./platform.js";

void runSidecar({
  channelType: CHANNEL_TYPE,
  build: (logger) => {
    const approvals = new LarkApprovals(logger);
    return new BotChannel<lark.LarkChannel, LarkChat>({
      channelType: CHANNEL_TYPE,
      logger,
      platform: new LarkPlatform(logger, approvals),
      approvals,
    });
  },
  register: runLarkRegistration,
});
