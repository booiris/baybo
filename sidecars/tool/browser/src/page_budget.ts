// A ceiling on how many tabs the browser accumulates.
//
// Nothing upstream reclaims a page. CDDM opens one per `new_page` and
// only ever closes one when the model explicitly calls `close_page`;
// there is no TTL, no LRU, no cap. Baybo makes that worse than it looks
// upstream: one sidecar serves every session, agent and subagent in a
// gateway process, so a tab outlives the agent that opened it and the
// count only falls when the whole browser is replaced. Observed in the
// field: `new_page` outnumbered `close_page` roughly 23:1.
//
// Each live tab is a Chrome renderer holding on the order of 100 MiB, so
// the honest failure mode is a slow climb toward the host's memory,
// interrupted only by a crash. This module puts a bound on it: before a
// new tab is opened, evict the least recently used ones.
//
// Recency is measured from *proxy traffic*, not from Chrome. Every tool
// call that names a `pageId` is a use, and so is the page a `new_page`
// just returned. Pages we have never seen used — left over from an
// earlier agent, or opened by the site itself — sort oldest, lowest id
// first, which is also their creation order (CDDM's page id is a
// monotonic counter, so ids are stable across closes and a lower id is
// strictly older).

import { createLogger } from "./log.js";

const logger = createLogger("[browser-mcp:pages]");

/**
 * Ceiling on concurrently open tabs. Not operator-configurable: this is a
 * backstop against unbounded growth, not a budget anyone should be tuning.
 * 128 is far above any plausible working set (a measured 7-tab browser sat
 * at 1.5 GiB), so it bites only when something has gone wrong — and a knob
 * would just invite someone to raise it past the point where the container
 * memory limit does the stopping instead.
 */
export const MAX_PAGES = 128;

export interface OpenPage {
  id: number;
  url: string;
  /** CDDM's process-global "current" page. After `new_page`, the new tab. */
  selected?: boolean;
}

export interface PageBudgetDeps {
  /** Always {@link MAX_PAGES} in production; a seam so tests need not open 129 tabs. */
  maxPages: number;
  /** Enumerate currently open pages. Rejecting disables enforcement for this call. */
  listPages: () => Promise<OpenPage[]>;
  /** Close one page by id. Rejecting is logged and skipped, never fatal. */
  closePage: (id: number) => Promise<void>;
}

export interface EvictionReport {
  closed: OpenPage[];
  /** Pages that were over budget but could not be closed. */
  failed: number[];
}

export class PageBudget {
  readonly #deps: PageBudgetDeps;
  /** pageId → monotonic use counter. Absent = never seen used by an agent. */
  readonly #lastUsed = new Map<number, number>();
  #clock = 0;

  constructor(deps: PageBudgetDeps) {
    this.#deps = deps;
  }

  get maxPages(): number {
    return this.#deps.maxPages;
  }

  /** Record that an agent touched this page. */
  noteUse(pageId: number): void {
    if (!Number.isInteger(pageId)) return;
    this.#clock += 1;
    this.#lastUsed.set(pageId, this.#clock);
  }

  /** Forget a page that is known to be gone, so its slot can't skew ordering. */
  noteClosed(pageId: number): void {
    this.#lastUsed.delete(pageId);
  }

  /**
   * Make room for one more tab. Evicts least-recently-used pages until
   * the open count is below the ceiling, leaving at least one page alive
   * (CDDM rejects closing the last one).
   *
   * Never throws: a browser that can't enumerate its pages is the
   * watchdog's problem, not a reason to fail the agent's navigation.
   */
  async makeRoomForOne(): Promise<EvictionReport> {
    const empty: EvictionReport = { closed: [], failed: [] };
    let open: OpenPage[];
    try {
      open = await this.#deps.listPages();
    } catch (e) {
      logger.debug(`page budget skipped: list_pages failed (${(e as Error).message})`);
      return empty;
    }
    this.#forgetVanished(open);

    // `open.length` is the count *before* the new tab, so the ceiling is
    // reached at maxPages - 1 survivors.
    const overBy = open.length - (this.#deps.maxPages - 1);
    if (overBy <= 0) return empty;
    const evictable = Math.min(overBy, open.length - 1);
    if (evictable <= 0) return empty;

    const victims = this.#leastRecentlyUsed(open).slice(0, evictable);
    const report: EvictionReport = { closed: [], failed: [] };
    for (const victim of victims) {
      try {
        await this.#deps.closePage(victim.id);
        this.#lastUsed.delete(victim.id);
        report.closed.push(victim);
      } catch (e) {
        logger.debug(`page budget: close_page ${victim.id} failed (${(e as Error).message})`);
        report.failed.push(victim.id);
      }
    }
    if (report.closed.length > 0) {
      logger.info(
        `page budget: ${open.length} open >= ${this.#deps.maxPages}; closed ${report.closed.length} ` +
          `least-recently-used (${report.closed.map((p) => p.id).join(",")})`,
      );
    }
    return report;
  }

  #forgetVanished(open: OpenPage[]): void {
    const live = new Set(open.map((p) => p.id));
    for (const id of [...this.#lastUsed.keys()]) {
      if (!live.has(id)) this.#lastUsed.delete(id);
    }
  }

  #leastRecentlyUsed(open: OpenPage[]): OpenPage[] {
    return [...open].sort((a, b) => {
      // 0 sorts before any real use counter, so never-used pages go first.
      const ua = this.#lastUsed.get(a.id) ?? 0;
      const ub = this.#lastUsed.get(b.id) ?? 0;
      if (ua !== ub) return ua - ub;
      return a.id - b.id;
    });
  }
}

/**
 * One line for the model, appended to the `new_page` result. Silent
 * eviction would read as "my tab vanished for no reason" the next time
 * the agent addressed a page id it still held.
 */
export function evictionNotice(report: EvictionReport, maxPages: number): string | undefined {
  if (report.closed.length === 0 && report.failed.length === 0) return undefined;
  const closed = report.closed
    .map((p) => `${p.id} (${p.url.length > 80 ? `${p.url.slice(0, 77)}...` : p.url})`)
    .join(", ");
  const failedPart =
    report.failed.length > 0 ? ` Could not close: ${report.failed.join(", ")}.` : "";
  const closedPart = report.closed.length > 0 ? ` Closed least-recently-used page(s): ${closed}.` : "";
  return (
    `Baybo note: this browser caps concurrently open tabs at ${maxPages} — every tab is a live ` +
    `Chrome renderer, and one sidecar serves every session in this gateway.${closedPart}${failedPart} ` +
    `Call close_page yourself when you are done with a page so the cap never has to choose for you.`
  );
}
