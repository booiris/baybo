/**
 * Env-var config loader. Every required var that's missing is reported
 * in a single error so the operator fixes them all at once instead of
 * playing startup whack-a-mole.
 *
 * The Telegram sidecar no longer takes a bot token via env — tokens
 * are provisioned by aura at runtime over the WS (`Frame::StartBot`),
 * so this module is just the allowlist + aura-facing connection env.
 */
export interface TelegramConfig {
  /**
   * Optional allowlist of Telegram numeric user IDs. Senders outside
   * the list get a polite refusal and their message is never forwarded
   * to aura. An empty set means "open bot — trust the Telegram layer".
   * Applies globally across every bot this sidecar runs; finer-grained
   * per-bot allowlists are a future addition if operators need them.
   */
  allowedUserIds: Set<number>;
}

export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConfigError";
  }
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): TelegramConfig {
  return {
    allowedUserIds: parseAllowlist(env["TELEGRAM_ALLOWED_USER_IDS"]),
  };
}

function parseAllowlist(raw: string | undefined): Set<number> {
  if (!raw) return new Set();
  const ids = new Set<number>();
  for (const part of raw.split(",")) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const id = Number(trimmed);
    if (!Number.isInteger(id) || id <= 0) {
      throw new ConfigError(
        `TELEGRAM_ALLOWED_USER_IDS: "${trimmed}" is not a positive integer`,
      );
    }
    ids.add(id);
  }
  return ids;
}
