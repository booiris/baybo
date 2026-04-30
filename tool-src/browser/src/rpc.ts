import { Packr, Unpackr } from "msgpackr";

// msgpackr defaults to a custom struct format that's not interoperable
// with rmp-serde's "named" maps. Forcing { useRecords: false } makes
// msgpackr emit standard maps that match `rmp_serde::to_vec_named` on
// the Rust side.
const packr = new Packr({ useRecords: false });
// Mirror the gateway's MAX_WS_MESSAGE_BYTES (16 MiB) on the sidecar
// side. msgpackr's Unpackr is willing to allocate strings/maps as
// large as the wire bytes it sees — a hostile or buggy gateway frame
// would otherwise OOM the sidecar before any application-level cap
// fires. The pre-decode byte gate in `decode()` is the primary fence;
// these maxArraySize/maxMapSize options are a defense-in-depth cap on
// pathological frames that *fit* under the byte limit but still
// declare obscene array/map counts.
export const MAX_WS_MESSAGE_BYTES = 16 * 1024 * 1024;
const unpackr = new Unpackr({
  useRecords: false,
  maxArraySize: 1_000_000,
  maxMapSize: 1_000_000,
});

export const PROTOCOL_VERSION = 1;

export type ToolFrame =
  | { kind: "register"; token: string; sidecar_id: string; protocol_version: number }
  | { kind: "register_ack"; ok: boolean; reason?: string | null }
  | { kind: "rpc_request"; id: string; method: string; params: Record<string, unknown> }
  | { kind: "rpc_result"; id: string; result: unknown }
  | { kind: "rpc_error"; id: string; code: string; message: string }
  | { kind: "event"; name: string; data: unknown }
  | { kind: "ping"; ts: number }
  | { kind: "pong"; ts: number };

export function encode(frame: ToolFrame): Buffer {
  return packr.pack(frame);
}

export function decode(bytes: Uint8Array): ToolFrame {
  if (bytes.byteLength > MAX_WS_MESSAGE_BYTES) {
    throw new Error(
      `tool-ws frame too large (${bytes.byteLength} > ${MAX_WS_MESSAGE_BYTES} bytes)`,
    );
  }
  const obj = unpackr.unpack(Buffer.from(bytes)) as ToolFrame;
  if (!obj || typeof (obj as { kind?: unknown }).kind !== "string") {
    throw new Error("decoded frame missing `kind` discriminant");
  }
  return obj;
}

export type RpcHandler = (
  params: Record<string, unknown> & { context_id: string },
) => Promise<unknown>;

/**
 * Dispatch an inbound `rpc_request` against a method table. Returns
 * the frame to write back (either `rpc_result` or `rpc_error`). Pure
 * function — exposed for unit tests.
 */
export async function dispatch(
  handlers: Record<string, RpcHandler>,
  req: Extract<ToolFrame, { kind: "rpc_request" }>,
): Promise<ToolFrame> {
  const handler = handlers[req.method];
  if (!handler) {
    return {
      kind: "rpc_error",
      id: req.id,
      code: "UNKNOWN_METHOD",
      message: `unknown method '${req.method}'`,
    };
  }
  const params = req.params as Record<string, unknown> & { context_id?: string };
  if (typeof params.context_id !== "string" || params.context_id.length === 0) {
    return {
      kind: "rpc_error",
      id: req.id,
      code: "MISSING_CONTEXT",
      message: "params.context_id is required",
    };
  }
  try {
    const result = await handler(params as Parameters<RpcHandler>[0]);
    return { kind: "rpc_result", id: req.id, result };
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    const code = e instanceof Error && e.name ? e.name : "EXECUTION_ERROR";
    return { kind: "rpc_error", id: req.id, code, message };
  }
}
