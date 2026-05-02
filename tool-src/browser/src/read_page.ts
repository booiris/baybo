// `browser/read_page` — extract clean Markdown from the currently
// selected Chrome page using Mozilla's Readability algorithm
// (Firefox Reader View) for boilerplate removal, then Turndown for
// HTML → Markdown conversion.
//
// Why this exists: chrome-devtools-mcp's `take_snapshot` returns the
// accessibility tree (verbose, hundreds of nodes for a typical article);
// `evaluate_script` returns whatever JS expression you pass. Neither
// gives the LLM a compact, prose-flavoured representation of the
// article. `read_page` does — it strips nav/ads/sidebars/footers via
// Readability, then renders the cleaned HTML to Markdown so the LLM
// gets headings, lists, links, and code blocks preserved without raw
// HTML noise.
//
// Implementation split:
// - **In-page (Readability)**: runs against the live DOM via
//   `evaluate_script`. The Readability source is embedded into our
//   bundle at build time (see `esbuild.config.mjs`) and injected into
//   Chrome on each call. Chrome's DOM is the right input — handles
//   SPA-rendered content correctly without us re-implementing JS
//   evaluation.
// - **In-Node (Turndown)**: takes `article.content` (HTML) from the
//   page response and converts to Markdown. Turndown ships with its
//   own minimal HTML parser, no jsdom required, ~30 KB bundled.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import TurndownService from "turndown";

// Inlined by esbuild's `define` config — see `esbuild.config.mjs`. The
// declaration is a build-time global that resolves to the file contents
// of `node_modules/@mozilla/readability/Readability.js` at bundle time.
declare const __READABILITY_SOURCE__: string;

export const READ_PAGE_TOOL = {
    name: "read_page",
    description:
        "Extract the main article content from the currently selected page as clean " +
        "Markdown (Mozilla Readability + Turndown). Returns title, byline, excerpt, and " +
        "the article body with headings/lists/links preserved. Strips navigation, ads, " +
        "sidebars, and footers. Use this instead of `take_snapshot` when you want the " +
        "page's prose content for reading or summarisation; use `take_snapshot` when you " +
        "need the full DOM/accessibility tree for interaction. The page must already be " +
        "navigated to and rendered (call `navigate_page` and `wait_for` first).",
    inputSchema: {
        type: "object",
        properties: {},
        additionalProperties: false,
    },
} as const;

interface ArticleFromPage {
    title: string | null;
    byline: string | null;
    excerpt: string | null;
    siteName: string | null;
    lang: string | null;
    length: number;
    content: string;
    textContent: string;
}

const turndown = new TurndownService({
    headingStyle: "atx",
    codeBlockStyle: "fenced",
    bulletListMarker: "-",
    emDelimiter: "_",
});
// Strip out things Readability doesn't always remove that aren't useful
// to an LLM (`<style>`, `<script>` shouldn't appear in article.content
// but Readability sometimes leaves inline `<noscript>` boilerplate).
turndown.remove(["style", "script", "noscript"]);

export async function handleReadPage(client: Client): Promise<CallToolResult> {
    // Inject Readability into the page (idempotent across calls thanks
    // to the `window.__auraReadability` cache), parse the live DOM, and
    // return the article object.
    //
    // SECURITY: the only interpolation into this script is the
    // bundle-time-fixed Readability source, and it goes through
    // `JSON.stringify` so it lands as a quoted string literal — the
    // in-page side then `new Function('module', src)` to execute it.
    // This means *any future interpolation* using the same shape would
    // also get escape-quoted by JSON.stringify, so accidentally piping
    // page or user data through here can no longer become a code-
    // injection vector. **Do not change this template literal to embed
    // anything via bare `${...}` interpolation** — always JSON.stringify
    // the value first, even if the source is "obviously trusted" today.
    //
    // Why `new Function('module', src)` (and not `eval`):
    // `new Function` creates a fresh global-scope closure, so the
    // injected code can't see surrounding `try`/`return` and can't
    // accidentally reference our locals. Readability.js ends with
    // `if (typeof module === "object") module.exports = Readability`,
    // so passing `moduleShim` as the `module` parameter triggers the
    // commonjs export branch and we recover the constructor via
    // `moduleShim.exports`.
    const readabilitySrcLiteral = JSON.stringify(__READABILITY_SOURCE__);
    const inPageScript = `
async () => {
  try {
    if (typeof window.__auraReadability === "undefined") {
      const moduleShim = { exports: undefined };
      const src = ${readabilitySrcLiteral};
      (new Function("module", src))(moduleShim);
      window.__auraReadability = moduleShim.exports;
      if (typeof window.__auraReadability !== "function") {
        return { __error: "Readability injection failed: module.exports is " + typeof moduleShim.exports };
      }
    }
    const doc = document.cloneNode(true);
    const reader = new window.__auraReadability(doc);
    const article = reader.parse();
    if (!article) return { __error: "Readability returned no article (page may be too short or non-prose)" };
    return {
      title: article.title || null,
      byline: article.byline || null,
      excerpt: article.excerpt || null,
      siteName: article.siteName || null,
      lang: article.lang || null,
      length: article.length || 0,
      content: article.content || "",
      textContent: article.textContent || "",
    };
  } catch (e) {
    return { __error: "Readability threw: " + (e && e.message ? e.message : String(e)) };
  }
}`;

    const cddmResult = await client.callTool({
        name: "evaluate_script",
        arguments: { function: inPageScript },
    });

    const article = extractArticleFromCddmResult(cddmResult);
    if ("__error" in article) {
        return {
            content: [{ type: "text", text: `read_page failed: ${article.__error}` }],
            isError: true,
        };
    }

    const markdown = turndown.turndown(article.content);
    const header: string[] = [];
    if (article.title) header.push(`# ${article.title}`);
    if (article.byline) header.push(`*${article.byline}*`);
    if (article.siteName) header.push(`Source: ${article.siteName}`);
    if (article.excerpt) header.push(`> ${article.excerpt}`);

    const body = [...header, "", markdown.trim()].join("\n").trim();

    return {
        content: [{ type: "text", text: body }],
        // Keep machine-readable metadata alongside the prose; some
        // clients consume `_meta` for downstream processing without
        // having to re-parse the Markdown header.
        _meta: {
            title: article.title,
            byline: article.byline,
            excerpt: article.excerpt,
            siteName: article.siteName,
            lang: article.lang,
            length: article.length,
        },
    };
}

function extractArticleFromCddmResult(
    result: unknown,
): ArticleFromPage | { __error: string } {
    // CDDM's evaluate_script wraps the result in MCP text content like:
    //   "Script ran on page and returned:\n```json\n<JSON.stringify(value)>\n```"
    // We parse the embedded JSON value out and shape-check it.
    const r = result as CallToolResult;
    if (r.isError) {
        return { __error: `evaluate_script failed: ${stringifyContent(r.content)}` };
    }
    const text = stringifyContent(r.content);
    const fenced = text.match(/```(?:json)?\n([\s\S]*?)\n```/);
    if (!fenced || !fenced[1]) {
        return { __error: `unexpected evaluate_script response shape: ${text.slice(0, 200)}` };
    }
    let parsed: unknown;
    try {
        // CDDM JSON.stringifies the function's return value — so the
        // fenced block contains a JSON string of the actual return.
        // Parse it once to unwrap.
        parsed = JSON.parse(fenced[1]);
    } catch (e) {
        return { __error: `JSON parse failed: ${(e as Error).message}` };
    }
    if (typeof parsed !== "object" || parsed === null) {
        return { __error: `expected object from in-page script, got ${typeof parsed}` };
    }
    if ("__error" in parsed && typeof (parsed as { __error: unknown }).__error === "string") {
        return { __error: (parsed as { __error: string }).__error };
    }
    return parsed as ArticleFromPage;
}

function stringifyContent(content: CallToolResult["content"]): string {
    return content
        .map((block) => {
            if (block.type === "text") return block.text;
            return `[${block.type} block]`;
        })
        .join("\n");
}
