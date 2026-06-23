import { RunnerError, type Channel, type RunOptions } from "./channel.js";
import { defaultLogger, type Logger } from "./logger.js";
import {
  runRegistration,
  type RegistrationContext,
  type RegistrationResult,
} from "./register.js";
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
   * so the channel can consume it (lines emit as NDJSON on stdout/
   * stderr, where the gateway's pipe drain parses them back into
   * structured log records). May be async for config loading, network
   * probes, etc. Any thrown error — including a missing env var — is
   * reported as fatal and exits 1.
   */
  build(logger: Logger): Channel | Promise<Channel>;

  /** Override the default shutdown-signal list. */
  signals?: NodeJS.Signals[];

  /**
   * Extra `runChannel` options. `abortSignal` and `logger` are owned
   * by `runSidecar` and cannot be overridden here — use `runChannel`
   * directly if you need that level of control.
   */
  runOptions?: Omit<RunOptions, "abortSignal" | "logger">;

  /**
   * One-shot registration hook. When the bundle is spawned with
   * `BAYBO_CHANNEL_MODE=register` the sidecar skips the normal
   * `build`/`runChannel` path and instead runs this function over a
   * stdin/stdout JSON protocol. Human-visible output (QR, progress)
   * should go to `process.stderr`; stdout is reserved for protocol
   * frames.
   */
  register(ctx: RegistrationContext): Promise<RegistrationResult>;
}

/**
 * Boilerplate wrapper for a sidecar's entry point.
 *
 * Handles everything every sidecar would otherwise write by hand:
 * constructs the SDK's NDJSON-on-stdout default logger, installs
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
 * import { runSidecar } from "@baybo/channel-sdk";
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
  if (process.env["BAYBO_CHANNEL_MODE"] === "register") {
    if (!opts.register) {
      const line =
        JSON.stringify({
          type: "error",
          message: `${opts.channelType}: no register hook declared`,
        }) + "\n";
      process.stdout.write(line, () => process.exit(1));
      return new Promise<never>(() => { });
    }
    await runRegistration(opts.register);
    // runRegistration is `Promise<never>` — it always exits — but guard
    // against future loosening so we never fall through into runChannel
    // after a successful registration.
    return new Promise<never>(() => { });
  }

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
