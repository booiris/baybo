// Shared stderr logging for the browser tool sidecar.
//
// MCP speaks JSON-RPC over stdout, so every log line — plain or NDJSON —
// must go to stderr, where the gateway's drain reads it
// (crates/tools/src/mcp/transport.rs). A line that parses as the shared
// `{"level","msg"}` NDJSON shape is re-emitted at its declared level; a
// plain-text line is forwarded verbatim at `info!`.
//
// So: `info` stays plain text (what the gateway has always seen from this
// sidecar), steady-state per-boot chatter goes out as `debug` to sit below
// the default INFO filter, and `warn` / `error` finally reach the operator
// at the level they deserve instead of being flattened into `info!`.

export interface SidecarLogger {
    info(msg: string): void;
    debug(msg: string): void;
    warn(msg: string): void;
    error(msg: string): void;
}

function write(line: string): void {
    try {
        process.stderr.write(line);
    } catch {
        // EPIPE / closed stream — nothing useful to do from inside a logger.
    }
}

function writeNdjson(level: "debug" | "warn" | "error", msg: string): void {
    let line: string;
    try {
        line = `${JSON.stringify({ level, msg })}\n`;
    } catch {
        return;
    }
    write(line);
}

export function createLogger(prefix: string): SidecarLogger {
    const tag = (msg: string): string => `${prefix} ${msg}`;
    return {
        info: (msg) => write(`${tag(msg)}\n`),
        debug: (msg) => writeNdjson("debug", tag(msg)),
        warn: (msg) => writeNdjson("warn", tag(msg)),
        error: (msg) => writeNdjson("error", tag(msg)),
    };
}

/** Narrow an unknown thrown value to a message string. */
export function errText(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
}
