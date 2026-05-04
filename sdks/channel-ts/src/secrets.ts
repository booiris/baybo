import { randomUUID } from "crypto";
import type WebSocket from "ws";

import { encodeFrame, type Frame, type SecretOp } from "./wire.js";
import type { Logger } from "./logger.js";

/**
 * Per-bot encrypted secret storage backed by Aura's vault. Sidecars
 * obtain a {@link Secrets} via the top-level {@link secrets} factory
 * and scope it to a specific bot. The gateway validates `botId` against
 * its own {@link aura_storage::ChannelBotStore}, so a sidecar can never
 * reach into another bot's namespace even if it tries.
 *
 * The transport rides the same WebSocket as every other channel frame.
 * Only the names round-trip through the SDK; values stay in
 * `aura_security`'s AES-GCM-encrypted vault on the server.
 */
export interface Secrets {
  /**
   * Scope to a specific `bot_id`. Calls against the returned scope
   * are validated server-side; an unknown / soft-deleted bot rejects
   * with {@link SecretBotUnknownError}.
   */
  scope(botId: string): SecretsScope;
}

export interface SecretsScope {
  /** `null` when the key isn't set. */
  get(key: string): Promise<string | null>;
  /** Stores `value` (≤ 64 KiB UTF-8 bytes). Idempotent on repeats. */
  set(key: string, value: string): Promise<void>;
  /** Soft-deletes the key. Idempotent on already-missing names. */
  delete(key: string): Promise<void>;
  /** Names of every live key under the scope. Order is unspecified. */
  list(): Promise<string[]>;
}

/** Thrown when the gateway didn't advertise the `"secrets"` capability. */
export class CapabilityMissingError extends Error {
  constructor(public readonly capability: string) {
    super(`gateway did not advertise capability '${capability}'`);
    this.name = "CapabilityMissingError";
  }
}

/** Sidecar tried to reach a bot the gateway has no live row for. */
export class SecretBotUnknownError extends Error {
  constructor(public readonly botId: string) {
    super(`gateway has no live bot row for '${botId}'`);
    this.name = "SecretBotUnknownError";
  }
}

/** Per-scope key budget exhausted. */
export class SecretQuotaExceededError extends Error {
  constructor(public readonly botId: string) {
    super(`per-bot secret quota exceeded for '${botId}'`);
    this.name = "SecretQuotaExceededError";
  }
}

const REQUEST_TIMEOUT_MS = 5_000;

interface PendingRequest {
  resolve: (frame: Frame & { kind: "secret_reply" }) => void;
  reject: (err: unknown) => void;
  timer: ReturnType<typeof setTimeout>;
}

/**
 * Internal correlator: writes `secret_request` frames onto the WS,
 * matches their `request_id` against incoming `secret_reply` frames,
 * and rejects on close / timeout. One per WS connection.
 */
export class SecretRouter {
  private readonly pending = new Map<string, PendingRequest>();
  private closed = false;

  constructor(
    private readonly ws: WebSocket,
    public readonly negotiated: ReadonlySet<string>,
    private readonly logger: Logger,
  ) {}

  hasCapability(capability: string): boolean {
    return this.negotiated.has(capability);
  }

  async request(args: {
    botId: string;
    op: SecretOp;
    key?: string;
    value?: string;
  }): Promise<Frame & { kind: "secret_reply" }> {
    if (!this.negotiated.has("secrets")) {
      throw new CapabilityMissingError("secrets");
    }
    if (this.closed) {
      throw new Error("secret router is closed (peer disconnected)");
    }
    const requestId = randomUUID();
    const frame: Frame = {
      kind: "secret_request",
      request_id: requestId,
      bot_id: args.botId,
      op: args.op,
      ...(args.key !== undefined ? { key: args.key } : {}),
      ...(args.value !== undefined ? { value: args.value } : {}),
    };
    return new Promise<Frame & { kind: "secret_reply" }>((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(requestId)) {
          reject(new Error(`secret ${args.op} timed out after ${REQUEST_TIMEOUT_MS}ms`));
        }
      }, REQUEST_TIMEOUT_MS);
      this.pending.set(requestId, { resolve, reject, timer });
      try {
        this.ws.send(encodeFrame(frame));
      } catch (err) {
        const entry = this.pending.get(requestId);
        if (entry) {
          this.pending.delete(requestId);
          clearTimeout(entry.timer);
        }
        reject(err);
      }
    });
  }

  /** Called by the runner when an incoming reply lands on the WS. */
  deliver(reply: Frame & { kind: "secret_reply" }): void {
    const entry = this.pending.get(reply.request_id);
    if (!entry) {
      this.logger.debug("secret reply for unknown request id", reply.request_id);
      return;
    }
    this.pending.delete(reply.request_id);
    clearTimeout(entry.timer);
    entry.resolve(reply);
  }

  /** Reject every in-flight request. Called when the WS disconnects. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error("secret router closed before reply"));
    }
    this.pending.clear();
  }
}

let activeRouter: SecretRouter | null = null;

export function installSecretsClient(router: SecretRouter): void {
  activeRouter = router;
}

/** Reset the active router only if it matches `previous` (prevents
 *  a late teardown on the previous connection from wiping a fresh
 *  one set up by an auto-reconnect). */
export function resetSecretsClient(previous: SecretRouter | null): void {
  if (activeRouter === previous) {
    activeRouter = null;
  }
}

/**
 * Public entry point. Returns a {@link Secrets} bound to the
 * currently-running channel runner. Call from inside `Channel.onStartBot`
 * (or any later hook) — it throws if invoked before the runner has
 * established its WebSocket. The `Secrets` value remains valid across
 * reconnects: each new connection installs a fresh router but the
 * surface stays the same.
 */
export function secrets(): Secrets {
  return {
    scope(botId: string): SecretsScope {
      return new SecretsScopeImpl(botId);
    },
  };
}

class SecretsScopeImpl implements SecretsScope {
  constructor(private readonly botId: string) {}

  async get(key: string): Promise<string | null> {
    const reply = await this.send({ op: "get", key });
    return reply.value ?? null;
  }

  async set(key: string, value: string): Promise<void> {
    await this.send({ op: "set", key, value });
  }

  async delete(key: string): Promise<void> {
    await this.send({ op: "delete", key });
  }

  async list(): Promise<string[]> {
    const reply = await this.send({ op: "list" });
    return reply.keys ?? [];
  }

  private async send(args: {
    op: SecretOp;
    key?: string;
    value?: string;
  }): Promise<Frame & { kind: "secret_reply" }> {
    const router = activeRouter;
    if (!router) {
      throw new Error(
        "secrets() called before the channel runner registered — wait for the WS handshake first",
      );
    }
    const reply = await router.request({
      botId: this.botId,
      op: args.op,
      ...(args.key !== undefined ? { key: args.key } : {}),
      ...(args.value !== undefined ? { value: args.value } : {}),
    });
    if (reply.ok) return reply;
    const error = reply.error ?? "internal";
    if (error === "bot_unknown") {
      throw new SecretBotUnknownError(this.botId);
    }
    if (error === "quota_exceeded") {
      throw new SecretQuotaExceededError(this.botId);
    }
    throw new Error(`secret ${args.op} failed: ${error}`);
  }
}

