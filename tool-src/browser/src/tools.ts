// Tool descriptor table for the embedded browser MCP server.
//
// One entry per LLM-visible browser tool. Each entry carries the
// rmcp-shaped descriptor (name, description, inputSchema) plus an
// Aura-specific `_meta.aura` block describing per-call approval policy
// (`access_rule`), the human-readable label shown in approval prompts
// (`call_label`), and which input field the gateway should auto-fill
// with the agent's session id (`inject_session_id_as`).
//
// Every browser tool routes per-Aura-session BrowserContexts via
// `context_id`, so every entry sets `inject_session_id_as: "context_id"`.
// The LLM never sees `context_id` in the schema — Aura's MCP client
// injects it into the params before the call.
//
// The `_meta.aura` shape is generated from Rust by ts-rs — see
// `crates/tools/src/mcp/access_rule.rs` and the regen flow in
// `scripts/check-ts-bindings.sh`. A wire-shape change in Rust is a TS
// compile error here, so the protocol can't drift.

import type { AuraToolMeta } from "./generated/AuraToolMeta.js";

export type { AccessRule } from "./generated/AccessRule.js";
export type { CallLabelRule } from "./generated/CallLabelRule.js";
export type { ResourceKind } from "./generated/ResourceKind.js";
export type { AuraToolMeta };

/** The full `_meta` envelope: rmcp's spec carries an open record under
 * `_meta`, and Aura tucks its annotations under the `aura` key. Other
 * MCP namespaces stay untouched. */
export interface ToolMeta {
  aura: AuraToolMeta;
}

export interface BrowserToolDescriptor {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  _meta: ToolMeta;
}

const auraMeta = (partial: Partial<AuraToolMeta>): ToolMeta => ({
  aura: { inject_session_id_as: "context_id", ...partial },
});

const obj = (
  properties: Record<string, unknown>,
  required: string[] = [],
): Record<string, unknown> => ({
  type: "object",
  properties,
  required,
  additionalProperties: false,
});

export const TOOLS: BrowserToolDescriptor[] = [
  {
    name: "navigate",
    description:
      "Open a URL in the agent's isolated browser context (Playwright + " +
      "bundled Chromium, dedicated profile dir, no shared cookies with the " +
      "user's normal browser). Returns the page title, final URL after " +
      "redirects, and a compact accessibility-tree snapshot with `@eN` " +
      "element references the other `browser/*` tools (click, type, …) " +
      "take.\n\n" +
      "SSRF floor: only `http(s)`; literal-IP hosts in loopback / RFC1918 / " +
      "link-local / metadata / unique-local ranges are rejected. The browser " +
      "context is created lazily and persists across calls in the same " +
      "session until idle GC reaps it (10 min).",
    inputSchema: obj(
      {
        url: {
          type: "string",
          format: "uri",
          description: "The http(s) URL to open",
        },
      },
      ["url"],
    ),
    _meta: auraMeta({
      access_rule: { kind: "public_ip_in_url_param", param: "url" },
      call_label: { kind: "from_param", param: "url", max_chars: 120 },
    }),
  },

  {
    name: "snapshot",
    description:
      "Return a fresh accessibility-tree snapshot of the current page. " +
      "Each interactive element gets a fresh `@eN` ref the other `browser/*` " +
      "tools (click, type, …) take. By default returns a compact tree (~200 " +
      "nodes); pass `full=true` for the full page source — useful for pages " +
      "with deeply nested content the compact walk truncates.",
    inputSchema: obj({
      full: {
        type: "boolean",
        description:
          "If true, return the full uncompacted tree (default false: ~200 nodes)",
      },
    }),
    _meta: auraMeta({}),
  },

  {
    name: "click",
    description:
      "Click an element previously seen in a `browser/snapshot` (or the " +
      "snapshot returned by `browser/navigate`). `ref` is the `@eN` value " +
      "from that snapshot.",
    inputSchema: obj(
      {
        ref: {
          type: "string",
          description: "The `@eN` ref from the most recent snapshot",
        },
      },
      ["ref"],
    ),
    _meta: auraMeta({}),
  },

  {
    name: "type",
    description:
      "Type into an input field previously seen in a snapshot. `ref` is " +
      "the `@eN` value; `text` is the literal text to enter (replaces any " +
      "existing value).",
    inputSchema: obj(
      {
        ref: {
          type: "string",
          description: "The `@eN` ref from the most recent snapshot",
        },
        text: { type: "string", description: "The text to type" },
      },
      ["ref", "text"],
    ),
    _meta: auraMeta({}),
  },

  {
    name: "scroll",
    description:
      "Scroll the page by ~500px in the given direction. Use to bring " +
      "off-screen content into the viewport before re-snapshotting.",
    inputSchema: obj(
      {
        direction: {
          type: "string",
          enum: ["up", "down"],
          description: "'up' or 'down'",
        },
      },
      ["direction"],
    ),
    _meta: auraMeta({}),
  },

  {
    name: "back",
    description:
      "Navigate back in the browser history. Returns the new URL. " +
      "If there's nothing to go back to (or back navigation fails), " +
      "the URL stays the same.",
    inputSchema: obj({}),
    _meta: auraMeta({}),
  },

  {
    name: "press",
    description:
      "Press a single keyboard key (or modifier+key combo). `key` follows " +
      "Playwright's key syntax: `Enter`, `Tab`, `ArrowDown`, `Control+A`, …",
    inputSchema: obj(
      {
        key: {
          type: "string",
          description: "Key name (e.g. 'Enter', 'Tab', 'Control+A')",
        },
      },
      ["key"],
    ),
    _meta: auraMeta({}),
  },

  {
    name: "get_images",
    description:
      "Return the list of `<img>` elements on the current page (src, alt, " +
      "natural width/height). Skips inline `data:` images.",
    inputSchema: obj({}),
    _meta: auraMeta({}),
  },

  {
    name: "screenshot",
    description:
      "Capture a PNG screenshot of the current page and return it as an " +
      "image attachment the LLM can see on the next turn (provided the model " +
      "supports vision). Bytes are persisted in the blob store; both the LLM " +
      "and the user's channel receive a reference to the same blob.\n\n" +
      "`full_page=true` captures the full scrollable page; default captures " +
      "only the current viewport.",
    inputSchema: obj({
      full_page: {
        type: "boolean",
        description:
          "If true, capture the full scrollable page (default false: viewport only)",
      },
    }),
    _meta: auraMeta({
      // Always prompt: a screenshot can capture PII the user opened in
      // a previous step (an email tab, a banking dashboard, …) that
      // they did not intend the LLM to see. The cache key splits
      // viewport vs full-page so the user can grant the cheaper
      // variant without auto-approving the heavier exfil surface.
      access_rule: {
        kind: "always",
        resource: "exec_command",
        label_template: "browser_screenshot ({full_page?full page:viewport})",
      },
      call_label: {
        kind: "template",
        template: "{full_page?full page:viewport}",
      },
    }),
  },

  {
    name: "console",
    description:
      "Inspect the page's console buffer and (optionally) evaluate a " +
      "JavaScript expression in the page context.\n\n" +
      "Without `expression`, returns recent console logs and uncaught " +
      "errors. With `expression` set, executes the JS and returns the value " +
      "alongside any logs produced. With `clear=true`, drops the buffered " +
      "logs after returning the current state.\n\n" +
      "JS evaluation is privileged (can read cookies, mutate the DOM, " +
      "exfiltrate page content) and prompts for explicit approval before " +
      "each call.",
    inputSchema: obj({
      expression: {
        type: "string",
        description:
          "Optional JavaScript expression to evaluate in the page context",
      },
      clear: {
        type: "boolean",
        description:
          "If true, clear the console buffer after returning its current state",
      },
    }),
    _meta: auraMeta({
      access_rule: {
        kind: "if_param_present",
        param: "expression",
        resource: "exec_command",
        label_template: "browser_console: {expression}",
      },
      call_label: { kind: "from_param", param: "expression", max_chars: 80 },
    }),
  },

  {
    name: "dialog",
    description:
      "Respond to a pending JavaScript dialog (alert / confirm / prompt / " +
      "beforeunload). `action` is 'accept' or 'dismiss'. `prompt_text` " +
      "supplies the value for `prompt()` dialogs (ignored for others). " +
      "`dialog_id` targets a specific dialog when several are queued; " +
      "defaults to the most recent.",
    inputSchema: obj(
      {
        action: { type: "string", enum: ["accept", "dismiss"] },
        prompt_text: {
          type: "string",
          description: "Reply text for prompt() dialogs",
        },
        dialog_id: {
          type: "string",
          description: "Optional dialog id from a prior snapshot",
        },
      },
      ["action"],
    ),
    _meta: auraMeta({}),
  },

  {
    name: "cdp",
    description:
      "Send a raw Chrome DevTools Protocol command to the page's CDP " +
      "session. Returns the raw CDP response. For agent flows that need " +
      "below-the-Playwright-API access (network interception, performance " +
      "tracing, accessibility tree dumps, …).",
    inputSchema: obj(
      {
        method: {
          type: "string",
          description: "CDP method name, e.g. 'Page.captureScreenshot'",
        },
        params: {
          type: "object",
          description: "CDP method params (passed through verbatim)",
        },
      },
      ["method"],
    ),
    _meta: auraMeta({
      access_rule: {
        kind: "always",
        resource: "exec_command",
        label_template: "browser_cdp: {method}",
      },
      call_label: { kind: "from_param", param: "method", max_chars: 80 },
    }),
  },
];
