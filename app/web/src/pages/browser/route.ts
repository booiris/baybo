export const BROWSER_VIEW_ROUTE = '/browser';

/** For a plain `<a>` outside the router: the app runs under `HashRouter`
 *  (`main.tsx`), so a bare `/browser` path would land on the catch-all. */
export const BROWSER_VIEW_HREF = `#${BROWSER_VIEW_ROUTE}`;

/** MCP tools are registered as `<server>/<tool>`; the browser sidecar's
 *  server is `browser` (`BROWSER_MCP_SERVER_NAME` in baybo-tools). */
const BROWSER_TOOL_PREFIX = 'browser/';

export function isBrowserTool(toolName: string | undefined): boolean {
  return toolName?.startsWith(BROWSER_TOOL_PREFIX) ?? false;
}
