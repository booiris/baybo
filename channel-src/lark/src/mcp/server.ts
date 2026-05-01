/**
 * Stub MCP server for the Lark sidecar's gateway-tunnelled
 * `Frame::Mcp` envelopes (Phase 3.3 slice 1 foundation).
 *
 * Today this responds to every JSON-RPC request with a
 * `tools_not_yet_wired` error. The actual Feishu OAPI tools land in
 * Phase 3.3 slice 2, which swaps this stub for an MCP server hosted
 * on top of `@modelcontextprotocol/sdk` and the existing
 * `@larksuiteoapi/node-sdk`.
 *
 * Notifications (JSON-RPC messages without an `id`) are dropped on
 * the floor — there's nothing to notify yet, and the spec lets servers
 * silently ignore unknown notifications.
 */

import type { Logger, McpReplyHandle } from "@aura/channel-sdk";

const TEXT_DECODER = new TextDecoder("utf-8", { fatal: false });
const TEXT_ENCODER = new TextEncoder();

const TOOLS_NOT_WIRED = -32601; // JSON-RPC "Method not found"

export interface LarkMcpServerOpts {
  logger: Logger;
}

export class LarkMcpServer {
  constructor(private readonly opts: LarkMcpServerOpts) {}

  /**
   * Handle one inbound JSON-RPC envelope. The current stub is
   * deliberately permissive: any well-formed request gets a
   * "method not found" error, anything that doesn't parse is
   * dropped (the agent-side client will time out, which is the
   * correct behaviour for malformed traffic).
   */
  async handle(payload: Uint8Array, reply: McpReplyHandle): Promise<void> {
    let envelope: unknown;
    try {
      envelope = JSON.parse(TEXT_DECODER.decode(payload));
    } catch (err) {
      this.opts.logger.debug(
        `lark mcp: dropping unparseable envelope (${String(err)})`,
      );
      return;
    }
    if (typeof envelope !== "object" || envelope === null) return;
    const obj = envelope as Record<string, unknown>;
    // Notifications have no `id` field — drop silently per JSON-RPC spec.
    if (!("id" in obj)) return;
    const method = typeof obj["method"] === "string" ? obj["method"] : "<missing>";
    const errorEnvelope = {
      jsonrpc: "2.0",
      id: obj["id"] ?? null,
      error: {
        code: TOOLS_NOT_WIRED,
        message: `Lark MCP server stub: method '${method}' is not yet wired (Phase 3.3 slice 2 ports the OAPI tools).`,
      },
    };
    await reply.send(TEXT_ENCODER.encode(JSON.stringify(errorEnvelope)));
  }
}
