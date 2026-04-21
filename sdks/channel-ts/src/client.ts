import type { Frame, Message } from "./wire.js";
import { PROTOCOL_VERSION, decodeFrame, encodeFrame } from "./wire.js";

export class SdkError extends Error {
  constructor(
    message: string,
    public readonly kind:
      | "connect"
      | "registration_rejected"
      | "protocol_violation"
      | "peer_closed"
      | "encode"
      | "decode"
      | "transport",
  ) {
    super(message);
    this.name = "SdkError";
  }
}

type Resolver<T> = (value: T) => void;
type Rejecter = (err: unknown) => void;

class FrameQueue {
  private readonly frames: Frame[] = [];
  private readonly waiters: Array<{ resolve: Resolver<Frame>; reject: Rejecter }> = [];
  private error: unknown | null = null;
  private closed = false;

  push(frame: Frame): void {
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve(frame);
      return;
    }
    this.frames.push(frame);
  }

  fail(err: unknown): void {
    if (this.closed) return;
    this.error = err;
    this.closed = true;
    for (const w of this.waiters.splice(0)) w.reject(err);
  }

  pop(): Promise<Frame> {
    const ready = this.frames.shift();
    if (ready) return Promise.resolve(ready);
    if (this.error) return Promise.reject(this.error);
    if (this.closed) {
      return Promise.reject(new SdkError("peer closed", "peer_closed"));
    }
    return new Promise((resolve, reject) => {
      this.waiters.push({ resolve, reject });
    });
  }
}

export interface ConnectOptions {
  wsUrl: string;
  token: string;
  channelType: string;
}

/**
 * Client handle for a sidecar's WebSocket connection to Aura.
 *
 * Construction drives the Register / RegisterAck handshake. Post-connect
 * the three interesting operations are `send`, `recv`, `close`.
 */
export class Client {
  private constructor(
    private readonly ws: WebSocket,
    private readonly queue: FrameQueue,
  ) {}

  static async connect(opts: ConnectOptions): Promise<Client> {
    const ws = new WebSocket(opts.wsUrl);
    ws.binaryType = "arraybuffer";
    const queue = new FrameQueue();

    await new Promise<void>((resolve, reject) => {
      const onOpen = () => {
        ws.removeEventListener("open", onOpen);
        ws.removeEventListener("error", onErr);
        resolve();
      };
      const onErr = () => {
        ws.removeEventListener("open", onOpen);
        ws.removeEventListener("error", onErr);
        reject(new SdkError(`websocket connect failed: ${opts.wsUrl}`, "connect"));
      };
      ws.addEventListener("open", onOpen);
      ws.addEventListener("error", onErr);
    });

    ws.addEventListener("message", (ev: MessageEvent) => {
      if (!(ev.data instanceof ArrayBuffer)) {
        queue.fail(new SdkError("expected binary frame", "protocol_violation"));
        return;
      }
      try {
        queue.push(decodeFrame(new Uint8Array(ev.data)));
      } catch (err) {
        queue.fail(err instanceof Error ? err : new SdkError(String(err), "decode"));
      }
    });
    ws.addEventListener("close", () => {
      queue.fail(new SdkError("peer closed", "peer_closed"));
    });
    ws.addEventListener("error", () => {
      queue.fail(new SdkError("websocket transport error", "transport"));
    });

    const registerFrame: Frame = {
      kind: "register",
      token: opts.token,
      channel_type: opts.channelType,
      protocol_version: PROTOCOL_VERSION,
    };
    ws.send(encodeFrame(registerFrame));

    const ack = await queue.pop();
    if (ack.kind !== "register_ack") {
      throw new SdkError(`expected register_ack, got ${ack.kind}`, "protocol_violation");
    }
    if (!ack.ok) {
      throw new SdkError(
        `registration rejected: ${ack.reason ?? ""}`,
        "registration_rejected",
      );
    }

    return new Client(ws, queue);
  }

  async send(msg: Message): Promise<void> {
    const frame: Frame = { kind: "message", ...msg };
    this.ws.send(encodeFrame(frame));
  }

  async recv(): Promise<Message> {
    const frame = await this.queue.pop();
    if (frame.kind !== "message") {
      throw new SdkError(`expected message frame, got ${frame.kind}`, "protocol_violation");
    }
    const { kind: _kind, ...msg } = frame;
    return msg;
  }

  async close(): Promise<void> {
    this.ws.close();
  }
}
