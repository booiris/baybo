import type { JSONRPCMessage, MessageExtraInfo } from "@modelcontextprotocol/sdk/types.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";

import type { McpReplyHandle } from "@aura/channel-sdk";

const TEXT_DECODER = new TextDecoder("utf-8", { fatal: false });
const TEXT_ENCODER = new TextEncoder();

/**
 * MCP `Transport` implementation that bridges the gateway-tunnelled
 * `Frame::Mcp` envelopes into the `@modelcontextprotocol/sdk`
 * server's expected wire shape.
 *
 * The agent side (or the gateway, on its behalf) opens a tunnel and
 * pushes JSON-RPC envelopes through `Channel.onMcpEnvelope`. The
 * sidecar's dispatch maps `tunnel_id` to one of these transports and
 * calls [`feed`] for each inbound payload. The transport JSON-decodes
 * and forwards via the SDK's `onmessage` callback.
 *
 * Outbound: the SDK's protocol layer calls [`send`]. The transport
 * encodes and ships the payload back through `McpReplyHandle.send`,
 * which wraps it in a `Frame::Mcp` reply.
 *
 * Buffering: the SDK requires `start()` to be called before any
 * messages flow. If `feed` arrives before `start` (the very first
 * envelope races the server constructor) we hold messages in `inbox`
 * and drain them inside `start`.
 */
export class TunnelMcpTransport implements Transport {
  private readonly inbox: JSONRPCMessage[] = [];
  private started = false;
  private closed = false;

  onmessage?: <T extends JSONRPCMessage>(message: T, extra?: MessageExtraInfo) => void;
  onerror?: (error: Error) => void;
  onclose?: () => void;

  constructor(private readonly reply: McpReplyHandle) {}

  async start(): Promise<void> {
    if (this.started) return;
    this.started = true;
    while (this.inbox.length > 0 && !this.closed) {
      const msg = this.inbox.shift()!;
      this.dispatch(msg);
    }
  }

  async send(message: JSONRPCMessage): Promise<void> {
    if (this.closed) return;
    await this.reply.send(TEXT_ENCODER.encode(JSON.stringify(message)));
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.onclose?.();
  }

  /** Push one inbound envelope. Decodes JSON; malformed payloads are
   * surfaced via `onerror` and dropped — the SDK itself never sees a
   * non-JSON message. */
  feed(payload: Uint8Array): void {
    if (this.closed) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(TEXT_DECODER.decode(payload));
    } catch (err) {
      this.onerror?.(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    if (!isJsonRpcMessage(parsed)) {
      this.onerror?.(new Error("inbound MCP envelope is not a JSON-RPC message"));
      return;
    }
    if (!this.started) {
      this.inbox.push(parsed);
      return;
    }
    this.dispatch(parsed);
  }

  private dispatch(message: JSONRPCMessage): void {
    if (!this.onmessage) return;
    this.onmessage(message);
  }
}

function isJsonRpcMessage(value: unknown): value is JSONRPCMessage {
  return (
    typeof value === "object"
    && value !== null
    && (value as { jsonrpc?: unknown }).jsonrpc === "2.0"
  );
}
