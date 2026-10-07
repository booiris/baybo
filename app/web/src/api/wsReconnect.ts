// Exponential reconnect backoff shared by the dashboard's WebSockets:
// 1s, 2s, 4s, … capped at 30s, reset on a successful handshake.

export const RECONNECT_BASE_MS = 1000;
export const RECONNECT_MAX_MS = 30_000;

export class ReconnectBackoff {
  private attempt = 0;

  /** Delay before the next attempt; each call advances the ladder. */
  nextDelayMs(): number {
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** this.attempt, RECONNECT_MAX_MS);
    this.attempt += 1;
    return delay;
  }

  reset(): void {
    this.attempt = 0;
  }
}
