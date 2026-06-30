/**
 * Per-line byte cap. A sidecar stuck in a hot loop logging huge blobs
 * could otherwise fill the gateway's log file with noise. Mirrored on
 * the Rust side; whichever end sees the line first trims it.
 */
const MAX_LINE_BYTES = 1024;

/**
 * Rate-limit window + budget. A sidecar burst above `MAX_LINES_PER_WINDOW`
 * lines in any `WINDOW_MS` drops the overflow and emits one summary line
 * per window — the WS pump keeps running and the gateway's log file
 * stays readable.
 */
const WINDOW_MS = 1000;
const MAX_LINES_PER_WINDOW = 100;

export interface Logger {
  debug(msg: string, ...args: unknown[]): void;
  info(msg: string, ...args: unknown[]): void;
  warn(msg: string, ...args: unknown[]): void;
  error(msg: string, ...args: unknown[]): void;
}

export type LogLevel = "debug" | "info" | "warn" | "error";

/**
 * Default sidecar logger. Emits one NDJSON record per call to
 * `process.stdout` (debug/info) or `process.stderr` (warn/error).
 * When the sidecar runs as a child of the gateway's `SidecarSupervisor`
 * (the in-tree path), the gateway's per-channel pipe drain parses each
 * line back into structured fields (`level`, `target`, `msg`) and
 * forwards them into `LogBuffer` (admin `/v1/logs`) plus the
 * per-channel file. A non-JSON line falls back to a plain text record
 * at info/warn (depending on which stream it came in on).
 *
 * **Out-of-tree note.** A sidecar started under a non-gateway parent
 * (systemd, docker, foreman, …) speaks the same WS protocol but the
 * gateway never sees its stdout/stderr — the records land wherever
 * the host process manager captures them (`journalctl`, `docker logs`,
 * the wrapping shell), not in `/v1/logs`. There is no SDK-level
 * shortcut to forward them over the WS; see `docs/modules/channels.md`
 * "Out-of-tree sidecars and observability" for the operator workarounds.
 *
 * The `target` field is reserved for future per-component attribution;
 * the default logger never sets it. A custom logger that wants to tag
 * sub-components can produce its own NDJSON with `{level, target, msg}`.
 */
export function defaultLogger(_channelType: string): Logger {
  let windowStart = 0;
  let windowCount = 0;
  let droppedInWindow = 0;

  const emit = (level: LogLevel, msg: string): void => {
    const now = Date.now();
    if (now - windowStart > WINDOW_MS) {
      if (droppedInWindow > 0) {
        writeNdjson(
          "warn",
          `[baybo-sdk] dropped ${droppedInWindow} log lines (rate cap ${MAX_LINES_PER_WINDOW}/${WINDOW_MS}ms)`,
        );
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
    writeNdjson(level, truncate(msg, MAX_LINE_BYTES));
  };

  return {
    debug(msg, ...args) {
      emit("debug", formatLine(msg, args));
    },
    info(msg, ...args) {
      emit("info", formatLine(msg, args));
    },
    warn(msg, ...args) {
      emit("warn", formatLine(msg, args));
    },
    error(msg, ...args) {
      emit("error", formatLine(msg, args));
    },
  };
}

/**
 * Write one NDJSON record. warn/error go to stderr so they preserve
 * unix conventions — a plain `node bundle.mjs 2>err.log` still pulls
 * out only the loud lines. Falls back to no-op on serialise failure
 * so the logger itself never throws into the caller.
 */
function writeNdjson(level: LogLevel, msg: string): void {
  let line: string;
  try {
    line = `${JSON.stringify({ level, msg })}\n`;
  } catch {
    return;
  }
  const stream =
    level === "warn" || level === "error" ? process.stderr : process.stdout;
  try {
    stream.write(line);
  } catch {
    // EPIPE / closed stream — nothing useful to do from inside a logger
  }
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
