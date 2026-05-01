import type { BrowserManager } from "./manager.js";
import { assertSafeUrl } from "./network_policy.js";
import { buildSnapshot } from "./snapshot.js";

/**
 * Cap on `page.goto` / `page.goBack`. Playwright's implicit default is
 * 30 s; we surface it explicitly so an unresponsive target site fails
 * fast instead of hanging until either Playwright's invisible timer
 * fires or the gateway-side per-tool timeout kicks in (whichever the
 * caller set).
 */
const NAVIGATION_TIMEOUT_MS = 30_000;

/**
 * MCP `tools/call` content frame. Mirrors `RawContent` from the Rust
 * side: text becomes `{type: "text", text}`; an image becomes
 * `{type: "image", data: <base64>, mimeType}`. We don't import the
 * SDK's types here to keep this file framework-agnostic.
 */
export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mimeType: string };

export type CallResult = {
  content: ContentBlock[];
  isError?: boolean;
};

export type HandlerArgs = Record<string, unknown> & { context_id: string };
export type ToolHandler = (args: HandlerArgs) => Promise<CallResult>;

/**
 * Resolve a `@eN` ref to a Playwright locator. The `eN` id is minted
 * by `snapshot.ts::walkPageSource` as a per-context nonce-suffixed
 * attribute (`data-aura-ref-<hex>`) on the element itself, so
 * resolution is a plain CSS lookup against Playwright's public
 * `Page.locator` API.
 *
 * Refs are valid only against the **most recent** snapshot for a
 * given context — `walkPageSource` strips `refAttr` from every
 * element before relabeling.
 *
 * Uniqueness check: a malicious page may copy our nonce-suffixed
 * attribute onto an attacker-controlled element AFTER the snapshot,
 * producing two matches. We assert exactly one match before acting.
 */
async function resolveRef(
  state: { page: import("playwright").Page; refAttr: string },
  ref: string,
): Promise<import("playwright").Locator> {
  const trimmed = ref.startsWith("@") ? ref.slice(1) : ref;
  if (!/^[a-zA-Z0-9_-]+$/.test(trimmed)) {
    const err = new Error(`invalid ref '${ref}': expected '[a-zA-Z0-9_-]+' after optional '@'`);
    err.name = "INVALID_REF";
    throw err;
  }
  const loc = state.page.locator(`[${state.refAttr}="${trimmed}"]`);
  const n = await loc.count();
  if (n === 0) {
    const err = new Error(`ref '${ref}' not found in current snapshot`);
    err.name = "REF_NOT_FOUND";
    throw err;
  }
  if (n > 1) {
    const err = new Error(
      `ref '${ref}' matches ${n} elements; possible page-side ref pollution. ` +
        `Re-run browser/snapshot to re-mint refs.`,
    );
    err.name = "REF_AMBIGUOUS";
    throw err;
  }
  return loc;
}

/** JSON-encode an arbitrary handler return value as MCP text content. */
function asText(value: unknown): CallResult {
  return { content: [{ type: "text", text: JSON.stringify(value) }] };
}

export function buildHandlers(manager: BrowserManager): Record<string, ToolHandler> {
  return {
    async navigate(args) {
      const url = String(args["url"] ?? "");
      if (!url) throw badParams("navigate: url is required");
      try {
        await assertSafeUrl(url, manager.allowLoopback());
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        const err = new Error(`navigate: ${msg}`);
        err.name = "BLOCKED_BY_SSRF_POLICY";
        throw err;
      }
      const state = await manager.acquire(args.context_id);
      await state.page.goto(url, {
        waitUntil: "domcontentloaded",
        timeout: NAVIGATION_TIMEOUT_MS,
      });
      state.url = state.page.url();
      const sup = manager.supervisorSnapshot(args.context_id);
      const text = await buildSnapshot(state.page, sup, false, state.refAttr);
      const title = await state.page.title().catch(() => "");
      return asText({ title, url: state.url, snapshot: text });
    },

    async snapshot(args) {
      const full = Boolean(args["full"]);
      const state = await manager.acquire(args.context_id);
      const sup = manager.supervisorSnapshot(args.context_id);
      const text = await buildSnapshot(state.page, sup, full, state.refAttr);
      return asText({ text });
    },

    async click(args) {
      const ref = String(args["ref"] ?? "");
      if (!ref) throw badParams("click: ref is required");
      const state = await manager.acquire(args.context_id);
      const loc = await resolveRef(state, ref);
      await loc.click();
      return asText({ ok: true });
    },

    async type(args) {
      const ref = String(args["ref"] ?? "");
      const text = String(args["text"] ?? "");
      if (!ref) throw badParams("type: ref is required");
      const state = await manager.acquire(args.context_id);
      const loc = await resolveRef(state, ref);
      await loc.fill("");
      await loc.fill(text);
      return asText({ ok: true });
    },

    async scroll(args) {
      const direction = String(args["direction"] ?? "");
      if (direction !== "up" && direction !== "down") {
        throw badParams("scroll: direction must be 'up' or 'down'");
      }
      const state = await manager.acquire(args.context_id);
      const dy = direction === "down" ? 500 : -500;
      await state.page.evaluate((delta: number) => window.scrollBy(0, delta), dy);
      return asText({ ok: true });
    },

    async back(args) {
      const state = await manager.acquire(args.context_id);
      await state.page
        .goBack({ waitUntil: "domcontentloaded", timeout: NAVIGATION_TIMEOUT_MS })
        .catch(() => null);
      state.url = state.page.url();
      return asText({ url: state.url });
    },

    async press(args) {
      const key = String(args["key"] ?? "");
      if (!key) throw badParams("press: key is required");
      const state = await manager.acquire(args.context_id);
      await state.page.keyboard.press(key);
      return asText({ ok: true });
    },

    async get_images(args) {
      const state = await manager.acquire(args.context_id);
      const images = await state.page.evaluate(() => {
        const out: Array<{ src: string; alt: string; w: number; h: number }> = [];
        const imgs = document.querySelectorAll("img");
        for (const el of imgs) {
          const src = el.getAttribute("src") ?? "";
          if (!src || src.startsWith("data:")) continue;
          out.push({
            src,
            alt: el.getAttribute("alt") ?? "",
            w: el.naturalWidth,
            h: el.naturalHeight,
          });
        }
        return out;
      });
      return asText({ images });
    },

    async screenshot(args) {
      const fullPage = Boolean(args["full_page"]);
      const state = await manager.acquire(args.context_id);
      const buf = await state.page.screenshot({ fullPage, type: "png" });
      const url = state.page.url();
      // Two parts: a JSON text summary so the LLM has the URL +
      // approximate size in its tool-result, and the inline image bytes
      // the Aura content adapter pipes into the blob store and surfaces
      // to a vision-capable LLM via MultiModalText.
      return {
        content: [
          {
            type: "text",
            text: JSON.stringify({ url, bytes: buf.length }),
          },
          {
            type: "image",
            data: buf.toString("base64"),
            mimeType: "image/png",
          },
        ],
      };
    },

    async console(args) {
      const expression = args["expression"];
      const clear = Boolean(args["clear"]);
      const state = await manager.acquire(args.context_id);
      const logs = state.supervisor.consoleLog();
      let result: { ok: boolean; value?: unknown; error?: string } | null = null;
      if (typeof expression === "string" && expression.length > 0) {
        try {
          const value = await state.page.evaluate(expression);
          result = { ok: true, value };
        } catch (e) {
          result = { ok: false, error: e instanceof Error ? e.message : String(e) };
        }
      }
      const errors = logs.filter((l) => l.level === "error");
      if (clear) state.supervisor.clearConsole();
      return asText({ logs, errors, result });
    },

    async dialog(args) {
      const action = args["action"];
      if (action !== "accept" && action !== "dismiss") {
        throw badParams("dialog: action must be 'accept' or 'dismiss'");
      }
      const promptText = typeof args["prompt_text"] === "string" ? args["prompt_text"] : null;
      const dialogId = typeof args["dialog_id"] === "string" ? args["dialog_id"] : null;
      const state = await manager.acquire(args.context_id);
      const out = await state.supervisor.respond(action, promptText, dialogId);
      return asText(out);
    },

    async cdp(args) {
      const method = String(args["method"] ?? "");
      if (!method) throw badParams("cdp: method is required");
      const cdpParams = args["params"];
      const state = await manager.acquire(args.context_id);
      const session = await state.ctx.newCDPSession(state.page);
      try {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const out = await (session as any).send(method, cdpParams ?? {});
        return asText(out);
      } finally {
        await session.detach().catch(() => null);
      }
    },
  };
}

function badParams(message: string): Error {
  const err = new Error(message);
  err.name = "INVALID_PARAMS";
  return err;
}
