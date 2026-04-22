import { RunnerError, type Channel, type RunOptions } from "./channel.js";
import { defaultLogger, type WireCapableLogger } from "./logger.js";
import { runChannel } from "./runner.js";

/**
 * Shutdown signals wired to `AbortController.abort()` by default. A
 * sidecar that wants custom signals (e.g. ignoring `SIGINT` because
 * it's being spawned interactively) can override via
 * {@link SidecarOptions.signals}.
 */
const DEFAULT_SIGNALS: NodeJS.Signals[] = ["SIGINT", "SIGTERM"];

export interface SidecarOptions {
  /**
   * Channel identifier. Used to tag the default logger and as the
   * `channel_type` field on the `Register` frame when
   * `build()` returns a channel that hasn't set it itself.
   */
  channelType: string;

  /**
   * Construct the channel. Invoked after the default logger is ready
   * so the channel can consume it (and its log lines ride the SDK's
   * wire-forwarder once the WS handshake completes). May be async for
   * config loading, network probes, etc. Any thrown error — including
   * a missing env var — is reported as fatal and exits 1.
   */
  build(logger: WireCapableLogger): Channel | Promise<Channel>;

  /** Override the default shutdown-signal list. */
  signals?: NodeJS.Signals[];

  /**
   * Extra `runChannel` options. `abortSignal` and `logger` are owned
   * by `runSidecar` and cannot be overridden here — use `runChannel`
   * directly if you need that level of control.
   */
  runOptions?: Omit<RunOptions, "abortSignal" | "logger">;
}

/**
 * Boilerplate wrapper for a sidecar's entry point.
 *
 * Handles everything every sidecar would otherwise write by hand:
 * constructs the SDK's wire-forwarding default logger, installs
 * `SIGINT`/`SIGTERM` handlers that route through a shared
 * `AbortController`, invokes `runChannel`, and translates the
 * runner's outcomes into deterministic process exit codes:
 *
 * - Clean shutdown (abort signal → `runChannel` resolves) → exit 0.
 * - `RunnerError` — bad config, rejected registration, protocol
 *   violation — is logged with its `kind` tag and exits 1.
 * - Any other thrown error (including errors from `build()`) is
 *   logged and exits 1.
 *
 * Intended usage:
 *
 * ```ts
 * #!/usr/bin/env node
 * import { runSidecar } from "@aura/channel-sdk";
 * import { MyChannel } from "./channel.js";
 *
 * void runSidecar({
 *   channelType: "my-channel",
 *   build: (logger) => new MyChannel(logger),
 * });
 * ```
 *
 * Returns `Promise<never>` because every code path calls
 * `process.exit`.
 */
export async function runSidecar(opts: SidecarOptions): Promise<never> {
  const logger = defaultLogger(opts.channelType);
  const controller = new AbortController();

  const signals = opts.signals ?? DEFAULT_SIGNALS;
  for (const sig of signals) {
    process.once(sig, () => {
      logger.info(`${sig} received; shutting down`);
      controller.abort();
    });
  }

  try {
    const channel = await opts.build(logger);
    await runChannel(channel, {
      ...(opts.runOptions ?? {}),
      abortSignal: controller.signal,
      logger,
    });
    process.exit(0);
  } catch (err) {
    if (err instanceof RunnerError) {
      logger.error(`runner error (${err.kind}): ${err.message}`);
    } else if (err instanceof Error) {
      logger.error(`fatal: ${err.message}`);
    } else {
      logger.error(`fatal: ${String(err)}`);
    }
    process.exit(1);
  }
}
