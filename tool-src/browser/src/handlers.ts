import type { BrowserManager } from "./manager.js";
import { assertSafeUrl } from "./network_policy.js";
import type { RpcHandler } from "./rpc.js";
import { buildSnapshot } from "./snapshot.js";

/**
 * Resolve a `@eN` ref to a Playwright locator. The `eN` id is minted
 * by `snapshot.ts::walkPageSource` as a per-context nonce-suffixed
 * attribute (`data-aura-ref-<hex>`) on the element itself, so
 * resolution is a plain CSS lookup against Playwright's public
 * `Page.locator` API. No internal Playwright selector engines
 * involved.
 *
 * Refs are valid only against the **most recent** snapshot for a
 * given context — `walkPageSource` strips `refAttr` from every
 * element before relabeling, so a stale ref from snapshot N is no
 * longer in the DOM after snapshot N+1.
 *
 * Uniqueness check: a malicious page may copy our nonce-suffixed
 * attribute onto an attacker-controlled element AFTER the snapshot,
 * producing two matches. We assert exactly one match before acting,
 * so a duplicate-attribute attack fails with a structured error
 * rather than silently mistargeting the click.
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
        `Re-run browser_snapshot to re-mint refs.`,
    );
    err.name = "REF_AMBIGUOUS";
    throw err;
  }
  return loc;
}

export function buildHandlers(manager: BrowserManager): Record<string, RpcHandler> {
  return {
    async navigate(params) {
      const url = String((params as { url?: unknown }).url ?? "");
      if (!url) throw badParams("navigate: url is required");
      // Defence in depth: the Rust `browser_navigate` already calls
      // `validate_url_with`, but the route handler runs in an
      // entirely separate process so we re-check here too. This also
      // catches hostnames that resolve only to blocked IP ranges,
      // which the syntactic Rust check can't see.
      try {
        await assertSafeUrl(url, manager.allowLoopback());
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        const err = new Error(`navigate: ${msg}`);
        err.name = "BLOCKED_BY_SSRF_POLICY";
        throw err;
      }
      const state = await manager.acquire(params.context_id);
      await state.page.goto(url, { waitUntil: "domcontentloaded" });
      state.url = state.page.url();
      const sup = manager.supervisorSnapshot(params.context_id);
      const text = await buildSnapshot(state.page, sup, false, state.refAttr);
      const title = await state.page.title().catch(() => "");
      return { title, url: state.url, snapshot: text };
    },

    async snapshot(params) {
      const full = Boolean((params as { full?: unknown }).full);
      const state = await manager.acquire(params.context_id);
      const sup = manager.supervisorSnapshot(params.context_id);
      const text = await buildSnapshot(state.page, sup, full, state.refAttr);
      return { text };
    },

    async click(params) {
      const ref = String((params as { ref?: unknown }).ref ?? "");
      if (!ref) throw badParams("click: ref is required");
      const state = await manager.acquire(params.context_id);
      const loc = await resolveRef(state, ref);
      await loc.click();
      return { ok: true };
    },

    async type(params) {
      const ref = String((params as { ref?: unknown }).ref ?? "");
      const text = String((params as { text?: unknown }).text ?? "");
      if (!ref) throw badParams("type: ref is required");
      const state = await manager.acquire(params.context_id);
      const loc = await resolveRef(state, ref);
      await loc.fill("");
      await loc.fill(text);
      return { ok: true };
    },

    async scroll(params) {
      const direction = String((params as { direction?: unknown }).direction ?? "");
      if (direction !== "up" && direction !== "down") {
        throw badParams("scroll: direction must be 'up' or 'down'");
      }
      const state = await manager.acquire(params.context_id);
      const dy = direction === "down" ? 500 : -500;
      await state.page.evaluate((delta: number) => window.scrollBy(0, delta), dy);
      return { ok: true };
    },

    async back(params) {
      const state = await manager.acquire(params.context_id);
      await state.page.goBack({ waitUntil: "domcontentloaded" }).catch(() => null);
      state.url = state.page.url();
      return { url: state.url };
    },

    async press(params) {
      const key = String((params as { key?: unknown }).key ?? "");
      if (!key) throw badParams("press: key is required");
      const state = await manager.acquire(params.context_id);
      await state.page.keyboard.press(key);
      return { ok: true };
    },

    async get_images(params) {
      const state = await manager.acquire(params.context_id);
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
      return { images };
    },

    async screenshot(params) {
      const fullPage = Boolean((params as { full_page?: unknown }).full_page);
      const state = await manager.acquire(params.context_id);
      const buf = await state.page.screenshot({ fullPage, type: "png" });
      return { png_b64: buf.toString("base64"), url: state.page.url() };
    },

    async eval(params) {
      const expression = (params as { expression?: unknown }).expression;
      const clear = Boolean((params as { clear?: unknown }).clear);
      const state = await manager.acquire(params.context_id);
      const logs = state.supervisor.consoleLog();
      let result: { ok: boolean; value?: unknown; error?: string } | null = null;
      if (typeof expression === "string" && expression.length > 0) {
        try {
          // Pass the expression string straight through to
          // `page.evaluate`, which Playwright sends via CDP
          // `Runtime.evaluate({ expression, awaitPromise: true })`.
          // That gives us native eval semantics: multi-line works,
          // promises are awaited, the last expression's value is
          // returned. Statements (`return 1`, `let x = 1`) are not
          // valid input — agents that want to mutate without a
          // return value should wrap in an IIFE themselves.
          const value = await state.page.evaluate(expression);
          result = { ok: true, value };
        } catch (e) {
          result = { ok: false, error: e instanceof Error ? e.message : String(e) };
        }
      }
      const errors = logs.filter((l) => l.level === "error");
      if (clear) state.supervisor.clearConsole();
      return { logs, errors, result };
    },

    async dialog(params) {
      const action = (params as { action?: unknown }).action;
      if (action !== "accept" && action !== "dismiss") {
        throw badParams("dialog: action must be 'accept' or 'dismiss'");
      }
      const promptText =
        typeof (params as { prompt_text?: unknown }).prompt_text === "string"
          ? ((params as { prompt_text: string }).prompt_text)
          : null;
      const dialogId =
        typeof (params as { dialog_id?: unknown }).dialog_id === "string"
          ? ((params as { dialog_id: string }).dialog_id)
          : null;
      const state = await manager.acquire(params.context_id);
      return await state.supervisor.respond(action, promptText, dialogId);
    },

    async cdp(params) {
      const method = String((params as { method?: unknown }).method ?? "");
      if (!method) throw badParams("cdp: method is required");
      const cdpParams = (params as { params?: unknown }).params;
      const state = await manager.acquire(params.context_id);
      const session = await state.ctx.newCDPSession(state.page);
      try {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const result = await (session as any).send(method, cdpParams ?? {});
        return { success: true, method, result };
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
