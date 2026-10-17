/** MCP tools are registered as `<server>/<tool>`; the browser sidecar's
 *  server is `browser` (`BROWSER_MCP_SERVER_NAME` in baybo-tools). */
const BROWSER_TOOL_PREFIX = 'browser/';

export function isBrowserTool(toolName: string | undefined): boolean {
  return toolName?.startsWith(BROWSER_TOOL_PREFIX) ?? false;
}
