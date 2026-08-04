// Up-front amendments to CDDM's tool descriptions.
//
// `net_hints.ts` explains a failure after it happens; this explains the
// two things the model needs *before* it acts, in the only place it
// reliably reads: the tool description itself.
//
// 1. Tabs are a bounded, shared resource. CDDM's `new_page` description
//    is "Open a new tab and load a URL" — nothing about closing, and
//    nothing about the browser being shared. With no instruction the
//    model closes roughly one tab for every twenty-three it opens.
// 2. In docker mode the browser is not on the agent's machine. Saying so
//    on `new_page` costs one sentence and saves a round trip against a
//    `localhost` that was never going to resolve.
//
// Amending descriptions rather than editing the system prompt keeps the
// text out of every deployment that has `browser.enable=false`, and keeps
// it adjacent to the call it governs.

import { CONTAINER_WORK_DIR } from "./docker.js";
import type { HintContext } from "./net_hints.js";

interface DescribedTool {
  name: string;
  description?: string | undefined;
}

const NEW_PAGE_TOOL = "new_page";

function sharedBrowserNote(maxPages: number): string {
  return (
    `One browser serves every agent and session in this Baybo process, and every open tab is a ` +
    `live Chrome renderer costing on the order of 100 MiB. Call close_page as soon as you are ` +
    `done with a page, and prefer navigate_page to re-use a tab you already own. Past ${maxPages} ` +
    `open tabs the least recently used one is closed automatically — possibly another agent's — ` +
    `so treat that ceiling as a fault indicator, not an allowance.`
  );
}

function containerNote(ctx: HintContext): string | undefined {
  if (ctx.mode === "cdp_url") {
    return (
      `This browser is an operator-managed Chrome reached over CDP; it does not necessarily run ` +
      `on this machine, so local addresses and local paths may not resolve for it.`
    );
  }
  if (ctx.mode !== "docker") return undefined;
  const host =
    ctx.hostGatewayHint !== undefined
      ? `use \`${ctx.hostGatewayHint}\` for a server running on this machine (it must bind 0.0.0.0)`
      : `a server bound to this machine's loopback is unreachable`;
  const files =
    ctx.workMountSource !== undefined
      ? `Host files are visible only under \`${ctx.workMountSource}\`, which appears here as ` +
        `\`${CONTAINER_WORK_DIR}\` — a \`file://\` URL must use the \`${CONTAINER_WORK_DIR}\` path, ` +
        `not the host path.`
      : `Host files are not visible: \`file://\` URLs naming this machine's paths will not resolve.`;
  return (
    `This browser runs in a container with its own network and filesystem: \`localhost\` is the ` +
    `container, not this machine — ${host}. ${files}`
  );
}

/**
 * Append Baybo's operating notes to the tools they apply to. Tools we
 * have nothing to add to are returned untouched.
 */
export function augmentToolDescriptions<T extends DescribedTool>(
  tools: T[],
  ctx: HintContext,
  maxPages: number,
): T[] {
  const notes = [sharedBrowserNote(maxPages), containerNote(ctx)].filter(
    (n): n is string => n !== undefined,
  );
  if (notes.length === 0) return tools;
  const suffix = `\n\nBaybo: ${notes.join(" ")}`;
  return tools.map((t) =>
    t.name === NEW_PAGE_TOOL ? { ...t, description: `${t.description ?? ""}${suffix}` } : t,
  );
}
