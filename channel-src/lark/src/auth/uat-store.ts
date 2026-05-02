import type { SecretsScope } from "@aura/channel-sdk";

import type { DeviceFlowToken } from "./device-flow.js";

/** Persisted UAT bundle for one Lark user under one bot.
 *
 * `expiresAt` / `refreshExpiresAt` are absolute Unix-millis so the
 * refresh scheduler can compare against `Date.now()` directly without
 * having to remember when the token landed. */
export interface StoredUAToken {
  accessToken: string;
  refreshToken: string;
  expiresAt: number;
  refreshExpiresAt: number;
  scope: string;
  /** Unix-millis of the original device-flow grant — useful for telemetry
   * and for the auth card's "Authorized at" subtitle. */
  grantedAt: number;
}

const KEY_PREFIX = "uat/";

/** Wrap a per-bot {@link SecretsScope} with typed UAT helpers.
 *
 * Keys live under `uat/<userOpenId>` so the same bot can also store
 * non-UAT secrets in its scope without colliding (e.g. encryption keys
 * for an unrelated subsystem). Values are JSON-serialised
 * {@link StoredUAToken}. */
export class UATStore {
  constructor(private readonly secrets: SecretsScope) {}

  async get(userOpenId: string): Promise<StoredUAToken | null> {
    const raw = await this.secrets.get(keyFor(userOpenId));
    if (raw === null) return null;
    try {
      return parseStoredUAToken(JSON.parse(raw));
    } catch {
      return null;
    }
  }

  async set(userOpenId: string, value: StoredUAToken): Promise<void> {
    await this.secrets.set(keyFor(userOpenId), JSON.stringify(value));
  }

  /** Replace `userOpenId`'s stored token with the result of a fresh
   * device-flow grant (or a refresh roundtrip). Stamps `grantedAt`
   * with `now` so a fresh grant rotates the timestamp; pass an
   * existing one to preserve it across a refresh. */
  async storeGrant(
    userOpenId: string,
    token: DeviceFlowToken,
    grantedAtMs: number = Date.now(),
  ): Promise<StoredUAToken> {
    const now = Date.now();
    const stored: StoredUAToken = {
      accessToken: token.accessToken,
      refreshToken: token.refreshToken,
      expiresAt: now + token.expiresIn * 1000,
      refreshExpiresAt: now + token.refreshExpiresIn * 1000,
      scope: token.scope,
      grantedAt: grantedAtMs,
    };
    await this.set(userOpenId, stored);
    return stored;
  }

  async delete(userOpenId: string): Promise<void> {
    await this.secrets.delete(keyFor(userOpenId));
  }

  /** Return every userOpenId currently tracked by this store. */
  async listUsers(): Promise<string[]> {
    const keys = await this.secrets.list();
    return keys
      .filter((k) => k.startsWith(KEY_PREFIX))
      .map((k) => k.slice(KEY_PREFIX.length));
  }
}

function keyFor(userOpenId: string): string {
  if (userOpenId.length === 0) {
    throw new Error("UATStore: userOpenId must be non-empty");
  }
  return `${KEY_PREFIX}${userOpenId}`;
}

function parseStoredUAToken(raw: unknown): StoredUAToken | null {
  if (typeof raw !== "object" || raw === null) return null;
  const r = raw as Record<string, unknown>;
  if (
    typeof r.accessToken !== "string" ||
    typeof r.refreshToken !== "string" ||
    typeof r.expiresAt !== "number" ||
    typeof r.refreshExpiresAt !== "number" ||
    typeof r.scope !== "string" ||
    typeof r.grantedAt !== "number"
  ) {
    return null;
  }
  return {
    accessToken: r.accessToken,
    refreshToken: r.refreshToken,
    expiresAt: r.expiresAt,
    refreshExpiresAt: r.refreshExpiresAt,
    scope: r.scope,
    grantedAt: r.grantedAt,
  };
}
