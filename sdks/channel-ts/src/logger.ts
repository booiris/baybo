export interface Logger {
  debug(msg: string, ...args: unknown[]): void;
  info(msg: string, ...args: unknown[]): void;
  warn(msg: string, ...args: unknown[]): void;
  error(msg: string, ...args: unknown[]): void;
}

export type WireLogLevel = "debug" | "info" | "warn" | "error";

/**
 * Callback the runner hands to the default logger once a WebSocket is
 * up. Receives one already-formatted log line plus its level; the runner
 * encodes a `SidecarLog` frame and writes it to the wire.
 */
export type WireLogSink = (level: WireLogLevel, text: string) => void;

/**
 * Default-logger extension point used by {@link runChannel}. Third-party
 * loggers (pino / winston / etc.) don't implement this method, and the
 * runner leaves them alone — they remain stdout-only.
 */
export interface WireCapableLogger extends Logger {
  setWireSink(send: WireLogSink | null): void;
}

export function hasWireSink(logger: Logger): logger is WireCapableLogger {
  return (
    typeof (logger as Partial<WireCapableLogger>).setWireSink === "function"
  );
}

/**
 * Per-line byte cap. A sidecar stuck in a hot loop logging huge blobs
 * could otherwise fill the admin LogBuffer with noise. Mirrored on the
 * Rust side; whichever end sees the line first trims it.
 */
const MAX_LINE_BYTES = 1024;

/**
 * Rate-limit window + budget. A sidecar burst above `MAX_LINES_PER_WINDOW`
 * lines in any `WINDOW_MS` drops the overflow and emits one summary line
 * per window — the WS pump keeps running and the dashboard stays
 * readable.
 */
const WINDOW_MS = 1000;
const MAX_LINES_PER_WINDOW = 100;

export function defaultLogger(_channelType: string): WireCapableLogger {
  let sink: WireLogSink | null = null;
  let windowStart = 0;
  let windowCount = 0;
  let droppedInWindow = 0;

  const forward = (level: WireLogLevel, text: string): void => {
    if (!sink) return;
    const now = Date.now();
    if (now - windowStart > WINDOW_MS) {
      if (droppedInWindow > 0) {
        try {
          sink(
            "warn",
            `[aura-sdk] dropped ${droppedInWindow} log lines (rate cap ${MAX_LINES_PER_WINDOW}/${WINDOW_MS}ms)`,
          );
        } catch {
          // sink failures are transient; swallow so the logger itself
          // never throws into the caller's code path
        }
        droppedInWindow = 0;
      }
      windowStart = now;
      windowCount = 0;
    }
    if (windowCount >= MAX_LINES_PER_WINDOW) {
      droppedInWindow += 1;
      return;
    }
    windowCount += 1;
    const line = truncate(text, MAX_LINE_BYTES);
    try {
      sink(level, line);
    } catch {
      // swallow — runner loop will surface real transport errors
    }
  };

  // With a wire sink attached, the gateway also pipes the child's
  // stdout/stderr into the same LogBuffer — console writes here would
  // double-log. Fall back to console only when no sink is set.
  return {
    debug(msg, ...args) {
      const text = formatLine(msg, args);
      if (sink) {
        forward("debug", text);
      }
    },
    info(msg, ...args) {
      const text = formatLine(msg, args);
      if (sink) {
        forward("info", text);
      } else {
        console.log(text);
      }
    },
    warn(msg, ...args) {
      const text = formatLine(msg, args);
      if (sink) {
        forward("warn", text);
      } else {
        console.warn(text);
      }
    },
    error(msg, ...args) {
      const text = formatLine(msg, args);
      if (sink) {
        forward("error", text);
      } else {
        console.error(text);
      }
    },
    setWireSink(next) {
      sink = next;
      // reset the rate-limit window so a reconnect doesn't inherit a
      // partially-spent budget from the prior connection
      windowStart = 0;
      windowCount = 0;
      droppedInWindow = 0;
    },
  };
}

function formatLine(msg: string, args: unknown[]): string {
  if (args.length === 0) return msg;
  const tail = args
    .map((a) => {
      if (typeof a === "string") return a;
      if (a instanceof Error) return a.stack ?? a.message;
      try {
        return JSON.stringify(a);
      } catch {
        return String(a);
      }
    })
    .join(" ");
  return `${msg} ${tail}`;
}

function truncate(text: string, maxBytes: number): string {
  // UTF-16 string length is a fast upper-bound proxy for byte length
  // in the common ASCII case; fall through to a byte-accurate scan
  // only when the line is big enough to matter.
  if (text.length <= maxBytes) return text;
  const encoder = new TextEncoder();
  const bytes = encoder.encode(text);
  if (bytes.length <= maxBytes) return text;
  // Trim by bytes and decode with replacement; TextDecoder strips
  // partial code-points at the tail automatically.
  const head = new TextDecoder("utf-8").decode(bytes.subarray(0, maxBytes));
  return `${head} [...truncated]`;
}
