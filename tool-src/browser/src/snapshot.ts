import type { Page } from "playwright";
import type { SupervisorSnapshot } from "./supervisor.js";

const COMPACT_MAX_NODES = 200;

interface SnapshotNode {
  role: string;
  name: string;
  ref: string | null;
  children: SnapshotNode[];
}

interface SnapshotPayload {
  url: string;
  title: string;
  tree: SnapshotNode[];
  truncated: boolean;
}

/**
 * Build the agent-facing snapshot of a page. We do this entirely
 * through public Playwright APIs:
 *
 *   1. `page.evaluate` walks the DOM, assigns each interactive
 *      element a stable `data-aura-ref="eN"` attribute (only minted
 *      once per element across snapshots), and returns a structured
 *      tree of role/name/ref triples.
 *   2. The tree is rendered as indented text with `@eN` ref markers.
 *
 * `handlers.ts::locator` resolves a ref back to a Playwright locator
 * via `[data-aura-ref="..."]` — also a public selector. No internal
 * Playwright APIs in either direction.
 *
 * Output also carries the supervisor block (pending dialogs, recent
 * console errors, frame tree) so a single `browser_snapshot` call
 * gives the agent everything it needs to plan the next action.
 */
export async function buildSnapshot(
  page: Page,
  supervisorState: SupervisorSnapshot | null,
  full: boolean,
  refAttr: string,
): Promise<string> {
  // Budget enforcement now lives INSIDE the page-side walker — see
  // `walkPageSource`. Without that, a hostile page with a giant
  // DOM (or layout-blowing `innerText` call paths) could hang the
  // sidecar before any host-side truncation kicked in.
  const payload = await page.evaluate(walkPageSource, {
    refAttr,
    maxNodes: COMPACT_MAX_NODES,
    full,
  });

  const lines: string[] = [];
  lines.push(`URL: ${payload.url}`);
  if (payload.title) lines.push(`TITLE: ${payload.title}`);
  lines.push("");
  lines.push("ACCESSIBILITY:");
  const renderNode = (node: SnapshotNode, depth: number): void => {
    const indent = "  ".repeat(depth);
    const refTag = node.ref ? ` @${node.ref}` : "";
    const name = node.name ? ` ${JSON.stringify(node.name)}` : "";
    lines.push(`${indent}[${node.role}${refTag}]${name}`);
    for (const child of node.children) renderNode(child, depth + 1);
  };
  for (const node of payload.tree) renderNode(node, 0);
  if (payload.truncated) {
    lines.push(`  [... truncated; pass full=true for the rest ...]`);
  }

  if (supervisorState) {
    if (supervisorState.pending_dialogs.length > 0) {
      lines.push("");
      lines.push("PENDING_DIALOGS:");
      for (const d of supervisorState.pending_dialogs) {
        lines.push(`  - id=${d.id} type=${d.type} message=${JSON.stringify(d.message)}`);
      }
    }
    const recentErrors = supervisorState.console_log.filter(
      (e) => e.level === "error" || e.level === "warning",
    );
    if (recentErrors.length > 0) {
      lines.push("");
      lines.push("RECENT_CONSOLE_ERRORS:");
      for (const e of recentErrors.slice(-5)) {
        lines.push(`  [${e.level}] ${e.text}`);
      }
    }
    if (supervisorState.frame_tree.length > 1) {
      lines.push("");
      lines.push("FRAMES:");
      for (const f of supervisorState.frame_tree) {
        lines.push(`  - ${f.name ?? "(unnamed)"} ${f.url}`);
      }
    }
  }

  return lines.join("\n");
}

/**
 * Page-side function. Runs inside the browser context, NOT the
 * sidecar. Must be self-contained — no closure references, no
 * imports. Compiled output is shipped through `page.evaluate` as a
 * function literal.
 *
 * The walker uses a per-context `refAttr` (nonce-suffixed) so a
 * malicious page can't statically pre-pollute the attribute. We
 * also strip the attribute from EVERY element on the page before
 * relabeling — even ones we labeled in a prior walk — so refs are
 * fresh per snapshot. The handler then asserts uniqueness when
 * resolving a ref to a locator.
 */
function walkPageSource(args: {
  refAttr: string;
  maxNodes: number;
  full: boolean;
}): SnapshotPayload {
  const refAttr = args.refAttr;
  const maxNodes = args.maxNodes;
  const full = args.full;

  // Wipe any prior `refAttr` annotations on the page (ours or
  // attacker-supplied) before relabeling. Refs are scoped to a
  // single snapshot; agents that hold a ref across multiple
  // snapshots without re-snapshotting are responsible for the
  // staleness window themselves.
  //
  // Use `querySelector` in a loop rather than `querySelectorAll` so
  // we don't build a full NodeList of every annotated element up
  // front — on a hostile 100k-node page that buffer dominates
  // wall-clock time even before our walker budget kicks in. The
  // single-match form short-circuits per call. Cap at `maxNodes`
  // (or 10k, whichever is larger): the walker downstream still
  // bounds at `maxNodes`, and any stale refs beyond the cap are
  // caught by the uniqueness assertion in `resolveRef`
  // (handlers.ts) when the agent tries to act on them.
  const stripCap = Math.max(maxNodes, 10_000);
  for (let i = 0; i < stripCap; i++) {
    const el = document.querySelector(`[${refAttr}]`);
    if (!el) break;
    el.removeAttribute(refAttr);
  }

  const seq = { next: 1 };
  // Mutable counter + flag so the walker can self-bound. Once
  // `full=false` and `emitted >= maxNodes`, every recursive entry
  // returns null without descending further. A hostile page with
  // 10⁵ nodes therefore touches at most `maxNodes` elements.
  let emitted = 0;
  let truncated = false;

  const interactiveTags = new Set([
    "A",
    "BUTTON",
    "INPUT",
    "TEXTAREA",
    "SELECT",
    "SUMMARY",
  ]);
  const interactiveRoles = new Set([
    "link",
    "button",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "menuitem",
    "tab",
    "switch",
  ]);
  const containerRoles = new Set([
    "main",
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "form",
    "search",
    "region",
    "dialog",
    "list",
    "listitem",
    "table",
    "row",
    "cell",
    "heading",
    "article",
    "section",
    "tablist",
    "tabpanel",
  ]);

  function inferRole(el: Element): string | null {
    const explicit = el.getAttribute("role");
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    switch (tag) {
      case "a":
        return el.hasAttribute("href") ? "link" : null;
      case "button":
        return "button";
      case "input": {
        const t = ((el as HTMLInputElement).type || "text").toLowerCase();
        if (t === "submit" || t === "button" || t === "reset") return "button";
        if (t === "checkbox") return "checkbox";
        if (t === "radio") return "radio";
        if (t === "hidden") return null;
        return "textbox";
      }
      case "textarea":
        return "textbox";
      case "select":
        return "combobox";
      case "summary":
        return "button";
      case "h1":
      case "h2":
      case "h3":
      case "h4":
      case "h5":
      case "h6":
        return "heading";
      case "img":
        return el.getAttribute("alt") ? "image" : null;
      case "nav":
        return "navigation";
      case "main":
        return "main";
      case "header":
        return el.closest("article, section") ? null : "banner";
      case "footer":
        return el.closest("article, section") ? null : "contentinfo";
      case "aside":
        return "complementary";
      case "form":
        return "form";
      case "label":
        return null;
      case "ul":
      case "ol":
        return "list";
      case "li":
        return "listitem";
      case "table":
        return "table";
      case "tr":
        return "row";
      case "td":
      case "th":
        return "cell";
      default:
        return null;
    }
  }

  function accessibleName(el: Element): string {
    const aria = el.getAttribute("aria-label");
    if (aria) return aria.trim().slice(0, 200);
    const labelledBy = el.getAttribute("aria-labelledby");
    if (labelledBy) {
      const ids = labelledBy.split(/\s+/);
      const texts: string[] = [];
      for (const id of ids) {
        const t = document.getElementById(id);
        if (t) texts.push((t.textContent ?? "").trim());
      }
      const joined = texts.filter((s) => s.length > 0).join(" ");
      if (joined) return joined.slice(0, 200);
    }
    const tag = el.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
      const id = (el as HTMLElement).id;
      if (id) {
        const lbl = document.querySelector(`label[for="${cssEscape(id)}"]`);
        if (lbl) {
          const txt = (lbl.textContent ?? "").trim();
          if (txt) return txt.slice(0, 200);
        }
      }
      const placeholder = el.getAttribute("placeholder");
      if (placeholder) return placeholder.slice(0, 200);
      const value = (el as HTMLInputElement).value;
      if (value) return value.slice(0, 80);
    }
    if (tag === "IMG") {
      return (el.getAttribute("alt") ?? "").slice(0, 200);
    }
    // Use `textContent` (DOM walk) instead of `innerText` (forces
    // layout reflow). For a hostile page with deep / wide content,
    // `innerText` cost is O(rendering), `textContent` is O(N).
    // We only ever consume the first 200 chars anyway.
    const raw = el.textContent ?? "";
    return raw.length > 400 ? raw.slice(0, 400).trim().slice(0, 200) : raw.trim().slice(0, 200);
  }

  function cssEscape(s: string): string {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(s);
    }
    return s.replace(/[^a-zA-Z0-9_-]/g, "\\$&");
  }

  function isVisible(el: Element): boolean {
    if (!(el instanceof HTMLElement) && !(el instanceof SVGElement)) return false;
    if (!el.isConnected) return false;
    const cs = getComputedStyle(el);
    if (cs.display === "none" || cs.visibility === "hidden") return false;
    if (parseFloat(cs.opacity) === 0) return false;
    if (el instanceof HTMLElement) {
      const rect = el.getBoundingClientRect();
      if (rect.width === 0 && rect.height === 0) return false;
    }
    return true;
  }

  function ensureRef(el: Element): string {
    // We just stripped `refAttr` from every element on the page
    // (above), so we never inherit an attacker-supplied value. Each
    // ensureRef call mints a fresh ref.
    const r = `e${seq.next++}`;
    el.setAttribute(refAttr, r);
    return r;
  }

  function shouldEmit(el: Element, role: string | null): "interactive" | "container" | null {
    if (!role) return null;
    if (!isVisible(el)) return null;
    if (interactiveTags.has(el.tagName) || interactiveRoles.has(role)) return "interactive";
    if (containerRoles.has(role)) return "container";
    return null;
  }

  function budgetExhausted(): boolean {
    if (full) return false;
    if (emitted >= maxNodes) {
      truncated = true;
      return true;
    }
    return false;
  }

  function walk(el: Element): SnapshotNode | null {
    if (budgetExhausted()) return null;
    const role = inferRole(el);
    const kind = shouldEmit(el, role);
    if (!kind) {
      const children: SnapshotNode[] = [];
      for (const child of Array.from(el.children)) {
        if (budgetExhausted()) break;
        const c = walk(child);
        if (c) children.push(c);
      }
      // Bubble single-child collapses up to the parent. Returning
      // null here lets the parent's loop pick up our children directly.
      if (children.length === 0) return null;
      if (children.length === 1) return children[0]!;
      // Synthesize a transparent group so we don't lose siblings.
      return {
        role: "group",
        name: "",
        ref: null,
        children,
      };
    }
    const node: SnapshotNode = {
      role: role!,
      name: accessibleName(el),
      ref: kind === "interactive" ? ensureRef(el) : null,
      children: [],
    };
    emitted += 1;
    for (const child of Array.from(el.children)) {
      if (budgetExhausted()) break;
      const c = walk(child);
      if (c) node.children.push(c);
    }
    return node;
  }

  const out: SnapshotNode[] = [];
  if (document.body) {
    for (const child of Array.from(document.body.children)) {
      if (budgetExhausted()) break;
      const node = walk(child);
      if (node) out.push(node);
    }
  }
  return {
    url: location.href,
    title: document.title,
    tree: out,
    truncated,
  };
}
