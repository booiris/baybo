// Annotates Chrome's `net::ERR_*` navigation failures with the one piece
// of context the model structurally cannot have: *where the browser
// lives*.
//
// In docker mode Chrome runs in its own network namespace and its own
// mount namespace. `http://127.0.0.1:<port>` is the container's loopback,
// not the gateway host's, and a `file:///…` path naming a host directory
// resolves against a filesystem that only carries the handful of
// bind-mounts `spawnContainer` passes. Chrome reports both as ordinary
// failures — `ERR_CONNECTION_REFUSED`, `ERR_FILE_NOT_FOUND` — which read
// exactly like "the server is down" and "the file isn't there". A model
// that just wrote that file, or just started that server, has no way to
// tell the difference and will burn turns re-checking the wrong thing.
//
// So we rewrite the error text at the proxy boundary. `isError` is left
// alone: the watchdog's liveness accounting keys off it, and the browser
// did answer.

import { CONTAINER_FONTS_DIR, CONTAINER_PROFILE_DIR, CONTAINER_WORK_DIR } from "./docker.js";

export type BrowserMode = "cdp_url" | "docker" | "host";

/** Chrome error codes that mean "the host you asked for isn't reachable from here". */
const HOST_REACHABILITY_CODES = [
  "net::ERR_CONNECTION_REFUSED",
  "net::ERR_ADDRESS_UNREACHABLE",
  "net::ERR_CONNECTION_TIMED_OUT",
  "net::ERR_NAME_NOT_RESOLVED",
] as const;

/** Chrome error codes that mean "that path doesn't exist in my filesystem". */
const FILE_VISIBILITY_CODES = ["net::ERR_FILE_NOT_FOUND", "net::ERR_ACCESS_DENIED"] as const;

/**
 * Loopback / link-local literals whose meaning changes across a network
 * namespace. A bare hostname is deliberately not on this list — it
 * resolves through the container's resolver and usually means what the
 * model thinks it means.
 */
const LOOPBACK_HOSTS = ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"] as const;

export interface HintContext {
  mode: BrowserMode;
  /** Host path bind-mounted at {@link CONTAINER_WORK_DIR}, when the mount is active. */
  workMountSource?: string | undefined;
  /** Host path bind-mounted at {@link CONTAINER_PROFILE_DIR}. */
  profileMountSource?: string | undefined;
  /** Host path bind-mounted at {@link CONTAINER_FONTS_DIR}, when a font dir was found. */
  fontMountSource?: string | undefined;
  /** Docker bridge gateway address the host is reachable on, when known. */
  hostGatewayHint?: string | undefined;
}

interface TextBlock {
  type: string;
  text?: unknown;
}

function blockText(block: unknown): string | undefined {
  if (typeof block !== "object" || block === null) return undefined;
  const b = block as TextBlock;
  if (b.type !== "text" || typeof b.text !== "string") return undefined;
  return b.text;
}

function matchedCode(text: string, codes: readonly string[]): string | undefined {
  return codes.find((c) => text.includes(c));
}

/**
 * `file:///a/b/c` → `/a/b/c`. Returns undefined when the text carries no
 * file URL, so the caller can skip the path-specific half of the hint.
 */
function firstFilePath(text: string): string | undefined {
  const m = text.match(/file:\/\/(\/[^\s"'<>]*)/);
  if (!m || m[1] === undefined) return undefined;
  try {
    return decodeURIComponent(m[1]);
  } catch {
    // A malformed percent-escape is not worth failing the hint over.
    return m[1];
  }
}

/**
 * Whether `path` sits inside `mount`, comparing whole path components.
 * A bare `startsWith` would claim `/w/workspace/x` lives under `/w/work`
 * and hand the agent a remapped URL that is wrong rather than merely
 * unhelpful.
 */
function underMount(path: string, mount: string | undefined): boolean {
  if (mount === undefined) return false;
  const base = mount.endsWith("/") ? mount.slice(0, -1) : mount;
  return path === base || path.startsWith(`${base}/`);
}

/** True when the failing URL names a host that only means something inside this namespace. */
function targetsLoopback(text: string): boolean {
  const m = text.match(/https?:\/\/([^/\s"'<>]+)/i);
  const authority = m?.[1];
  if (authority === undefined) return false;
  const host = authority.replace(/:\d+$/, "").toLowerCase();
  return LOOPBACK_HOSTS.some((h) => h === host);
}

function reachabilityHint(text: string, ctx: HintContext): string {
  if (ctx.mode === "cdp_url") {
    return (
      `Baybo note: this browser is an operator-managed Chrome reached over CDP — it is not ` +
      `necessarily on this machine. A host/port that resolves for the agent may not resolve for it.`
    );
  }
  const gateway =
    ctx.hostGatewayHint !== undefined
      ? `Reach the gateway host at \`${ctx.hostGatewayHint}\` instead of \`localhost\`.`
      : `This container has no configured alias for the gateway host, so there may be no address ` +
        `that reaches it at all.`;
  const loopbackClause = targetsLoopback(text)
    ? `You asked for a loopback address, so this is almost certainly the namespace split rather ` +
      `than a dead server. `
    : "";
  return (
    `Baybo note: this browser runs in a Docker container with its own network namespace. ` +
    `${loopbackClause}\`localhost\` / \`127.0.0.1\` inside it are the *container*, not the gateway ` +
    `host, so a server you started on the host is invisible here. ${gateway} Two further ` +
    `conditions have to hold and neither is visible from the error: the server must bind ` +
    `\`0.0.0.0\` rather than \`127.0.0.1\`, and the host firewall must admit the docker bridge ` +
    `subnet. Check both before concluding the address was wrong.`
  );
}

function fileHint(text: string, ctx: HintContext): string {
  if (ctx.mode === "cdp_url") {
    return (
      `Baybo note: this browser is an operator-managed Chrome reached over CDP. It reads ` +
      `\`file://\` paths from its own filesystem, which is not necessarily this one.`
    );
  }
  const path = firstFilePath(text);
  const mounts: string[] = [];
  if (ctx.workMountSource !== undefined) {
    mounts.push(`${ctx.workMountSource} → ${CONTAINER_WORK_DIR} (read-only)`);
  }
  if (ctx.profileMountSource !== undefined) {
    mounts.push(`${ctx.profileMountSource} → ${CONTAINER_PROFILE_DIR}`);
  }
  // Conditional: the font dir is only mounted when one was found on the
  // host. Listing it unconditionally would send an agent chasing a path
  // that is empty and baked into the image, not a mount.
  if (ctx.fontMountSource !== undefined) {
    mounts.push(`${ctx.fontMountSource} → ${CONTAINER_FONTS_DIR} (read-only)`);
  }
  const mountList =
    mounts.length > 0
      ? `Host paths visible to it: ${mounts.join("; ")}. Nothing else on the host exists here.`
      : `No host path is mounted into it at all.`;

  const remap =
    path !== undefined && underMount(path, ctx.workMountSource)
      ? ` Rewrite this one as \`file://${CONTAINER_WORK_DIR}${path.slice((ctx.workMountSource ?? "").length)}\`.`
      : path !== undefined
        ? ` \`${path}\` is not under a mount. Either write the file under the work directory ` +
          `first, or copy it in with \`docker cp <path> <container>:/tmp/\` and navigate to ` +
          `\`file:///tmp/<name>\`.`
        : "";

  return (
    `Baybo note: this browser runs in a Docker container with its own filesystem — the path ` +
    `existing on the gateway host says nothing about whether Chrome can see it. ${mountList}${remap}`
  );
}

/**
 * Append a mode-aware explanation to a `net::ERR_*` tool result. Returns
 * the result unchanged when there is nothing useful to add — host mode,
 * a non-error result, or an error we have no better story for.
 */
export function annotateBrowserError<T extends object>(result: T, ctx: HintContext): T {
  if (ctx.mode === "host") return result;
  // Only a *failed* call gets the treatment. `net::ERR_*` appears in plenty
  // of successful results — `list_console_messages` on a page whose own
  // fetch failed, a snapshot of an error page, a network log — and lecturing
  // the agent about container namespaces there is both wrong and confusing.
  if ((result as { isError?: unknown }).isError !== true) return result;
  // The SDK's CallToolResult is a union that still carries the legacy
  // `{ toolResult }` arm, so `content` can't be read off the type.
  const content = (result as { content?: unknown }).content;
  if (!Array.isArray(content)) return result;

  const combined = content
    .map(blockText)
    .filter((t): t is string => t !== undefined)
    .join("\n");
  if (combined.length === 0) return result;

  const hint =
    matchedCode(combined, HOST_REACHABILITY_CODES) !== undefined
      ? reachabilityHint(combined, ctx)
      : matchedCode(combined, FILE_VISIBILITY_CODES) !== undefined
        ? fileHint(combined, ctx)
        : undefined;
  if (hint === undefined) return result;

  return {
    ...result,
    content: [...content, { type: "text", text: hint }],
  };
}
