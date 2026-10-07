// Read-only tap on chrome-devtools-mcp's browser context, for the screencast
// viewer (`screencast.ts`).
//
// CDDM builds an `McpContext` through the static `McpContext.from(browser, …)`
// every time it adopts a new puppeteer `Browser` (first tool call, relaunch
// after a crash, docker heal). The call site looks the static up at call time
// (`build/src/index.js`, `McpContext.from(browser, logger, …)`), so replacing
// the property on the class is enough to see every context CDDM creates —
// with its own `Browser`, page-id map and selected page — without a single
// CDDM tool call and without opening a second CDP connection.
//
// The tap sits on the agent's critical path, so it is fail-safe by
// construction:
// - it is installed only when the view link is configured;
// - CDDM's return value is handed back untouched (the very same promise);
// - the callback runs later, in a microtask, inside try/catch;
// - a CDDM whose version or shape differs from the one this was verified
//   against is not tapped at all — viewing degrades to "unavailable", tools
//   keep working.
//
// Module identity matters: the specifier `chrome-devtools-mcp/McpContext` is
// externalised by `esbuild.config.mjs` to the same `cddm/build/src/
// McpContext.js` URL `index.js` imports, so this patches the class CDDM uses.

import type { UnavailableReason } from "./generated/UnavailableReason.js";
import { errText, type SidecarLogger } from "./log.js";

/**
 * The CDDM release the tap was verified against (spike M0.1). A bump of the
 * `chrome-devtools-mcp` pin in package.json fails `test/cddm_tap.test.mjs`
 * until this is re-verified and updated.
 */
export const TAPPED_CDDM_VERSION = "1.1.0";

const TAP_MARKER: unique symbol = Symbol.for("baybo.browser.cddmContextTap");

type TapFn = ((...args: unknown[]) => unknown) & { [TAP_MARKER]?: true };

export type TapOutcome = { installed: true } | { installed: false; reason: UnavailableReason };

/** What {@link installCddmTap} needs from CDDM; injectable for tests. */
export interface CddmModules {
  mcpContext: unknown;
  version: unknown;
}

async function loadCddmModules(): Promise<CddmModules> {
  // Dynamic so a CDDM without these files fails here, inside the
  // link-only path, instead of at sidecar boot.
  const [ctxModule, versionModule] = await Promise.all([
    import("chrome-devtools-mcp/McpContext"),
    import("chrome-devtools-mcp/version"),
  ]);
  return { mcpContext: ctxModule.McpContext, version: versionModule.VERSION };
}

/**
 * Wrap `holder.from` so every value it resolves to is also handed to
 * `onContext`. Idempotent: a second call on an already tapped holder leaves
 * the first tap (and its callback) in place.
 */
export function tapContextFrom(
  holder: unknown,
  onContext: (ctx: unknown) => void,
  log: SidecarLogger,
): TapOutcome {
  const mismatch: TapOutcome = { installed: false, reason: "cddm_version_mismatch" };
  if ((typeof holder !== "function" && typeof holder !== "object") || holder === null) {
    return mismatch;
  }
  const target = holder as { from?: unknown };
  const original = target.from;
  if (typeof original !== "function") return mismatch;
  if ((original as TapFn)[TAP_MARKER] === true) return { installed: true };

  const deliver = (ctx: unknown): void => {
    queueMicrotask(() => {
      try {
        onContext(ctx);
      } catch (e) {
        log.warn(`screencast context tap callback failed: ${errText(e)}`);
      }
    });
  };

  const tapped: TapFn = function (this: unknown, ...args: unknown[]): unknown {
    const result: unknown = (original as (...a: unknown[]) => unknown).apply(this, args);
    try {
      // A side branch: CDDM awaits `result` itself; a rejection there is
      // CDDM's to report, so this branch only swallows its own copy.
      Promise.resolve(result).then(deliver, () => undefined);
    } catch {
      // Never let the tap turn a working call into a failing one.
    }
    return result;
  };
  tapped[TAP_MARKER] = true;

  try {
    target.from = tapped;
  } catch (e) {
    log.warn(`cannot patch McpContext.from: ${errText(e)}`);
    return mismatch;
  }
  if (target.from !== tapped) return mismatch;
  return { installed: true };
}

/**
 * Install the tap on the real CDDM, if it is the version the tap was
 * verified against. Never throws.
 */
export async function installCddmTap(
  onContext: (ctx: unknown) => void,
  log: SidecarLogger,
  load: () => Promise<CddmModules> = loadCddmModules,
): Promise<TapOutcome> {
  let modules: CddmModules;
  try {
    modules = await load();
  } catch (e) {
    log.warn(`browser viewer unavailable: cannot load CDDM internals (${errText(e)})`);
    return { installed: false, reason: "cddm_version_mismatch" };
  }
  if (modules.version !== TAPPED_CDDM_VERSION) {
    log.warn(
      `browser viewer unavailable: chrome-devtools-mcp ${String(modules.version)} is not the ` +
        `verified ${TAPPED_CDDM_VERSION}; browser tools are unaffected`,
    );
    return { installed: false, reason: "cddm_version_mismatch" };
  }
  const outcome = tapContextFrom(modules.mcpContext, onContext, log);
  if (!outcome.installed) {
    log.warn("browser viewer unavailable: McpContext.from is missing; browser tools are unaffected");
  }
  return outcome;
}
