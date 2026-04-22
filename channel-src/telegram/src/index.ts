#!/usr/bin/env node
import { runSidecar } from "@aura/channel-sdk";

import { TelegramChannel } from "./channel.js";
import { loadConfig } from "./config.js";

void runSidecar({
  channelType: "telegram",
  build: (logger) => new TelegramChannel(loadConfig(), logger),
});
