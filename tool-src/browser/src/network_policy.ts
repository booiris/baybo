import { promises as dns } from "node:dns";
import * as net from "node:net";

/**
 * Mirrors `aura_security::is_blocked_ip` (Rust). Used both for
 * pre-navigation URL checks and for per-request interception via
 * `page.route`. Both layers matter:
 *
 * - Pre-navigation: rejects `http://attacker.example` whose only
 *   A record is 169.254.169.254, before Chromium even opens a socket.
 * - Per-request: catches redirects and page-initiated subresource
 *   fetches that would otherwise sneak past a one-shot URL check.
 *
 * **Known TOCTOU window**: we resolve hostnames in Node and then let
 * Chromium make its own request. A DNS rebind that flips the answer
 * between the two lookups would slip through. For MVP we accept this
 * — closing it requires `route.fulfill` with a hand-fetched body,
 * which breaks SNI / Host semantics and is a substantial rewrite.
 */

export function isBlockedIp(addr: string, allowLoopback = false): boolean {
  const fam = net.isIP(addr);
  if (fam === 4) return isBlockedV4(addr, allowLoopback);
  if (fam === 6) return isBlockedV6(addr, allowLoopback);
  // Anything that's not a recognisable IP literal is suspicious.
  return true;
}

function isBlockedV4(addr: string, allowLoopback: boolean): boolean {
  const parts = addr.split(".").map((s) => Number(s));
  if (parts.length !== 4) return true;
  for (const p of parts) {
    if (!Number.isInteger(p) || p < 0 || p > 255) return true;
  }
  const [a, b, c, _d] = parts as [number, number, number, number];
  // Loopback 127.0.0.0/8.
  if (a === 127) return !allowLoopback;
  // Unspecified 0.0.0.0/8.
  if (a === 0) return true;
  // Private RFC1918.
  if (a === 10) return true;
  if (a === 172 && b >= 16 && b <= 31) return true;
  if (a === 192 && b === 168) return true;
  // CGNAT 100.64.0.0/10.
  if (a === 100 && b >= 64 && b <= 127) return true;
  // Link-local 169.254/16 (covers 169.254.169.254 metadata).
  if (a === 169 && b === 254) return true;
  // Multicast 224.0.0.0/4.
  if (a >= 224 && a <= 239) return true;
  // Reserved 240.0.0.0/4 + broadcast.
  if (a >= 240) return true;
  // Documentation ranges.
  if (a === 192 && b === 0 && c === 2) return true;
  if (a === 198 && b === 51 && c === 100) return true;
  if (a === 203 && b === 0 && c === 113) return true;
  return false;
}

function isBlockedV6(addr: string, allowLoopback: boolean): boolean {
  // Parse to 8 16-bit segments so per-prefix checks are bit-exact.
  // String regexes can't catch every form WHATWG URL canonicalises
  // an IPv6 literal into — Node turns `[::ffff:127.0.0.1]` into
  // `[::ffff:7f00:1]`, both have to map to the same v4 address.
  const segs = expandIpv6(addr);
  if (!segs) return true; // unparseable → conservative deny
  // IPv4-mapped /96: `0:0:0:0:0:ffff:HHHH:LLLL`.
  if (
    segs[0] === 0 &&
    segs[1] === 0 &&
    segs[2] === 0 &&
    segs[3] === 0 &&
    segs[4] === 0 &&
    segs[5] === 0xffff
  ) {
    const a = (segs[6]! >> 8) & 0xff;
    const b = segs[6]! & 0xff;
    const c = (segs[7]! >> 8) & 0xff;
    const d = segs[7]! & 0xff;
    return isBlockedV4(`${a}.${b}.${c}.${d}`, allowLoopback);
  }
  // Loopback ::1 (and the explicit-zeros canonical form).
  if (
    segs[0] === 0 &&
    segs[1] === 0 &&
    segs[2] === 0 &&
    segs[3] === 0 &&
    segs[4] === 0 &&
    segs[5] === 0 &&
    segs[6] === 0 &&
    segs[7] === 1
  ) {
    return !allowLoopback;
  }
  // Unspecified ::.
  if (segs.every((s) => s === 0)) return true;
  // Link-local fe80::/10 → top 10 bits == 1111111010.
  if ((segs[0]! & 0xffc0) === 0xfe80) return true;
  // Unique-local fc00::/7 → top 7 bits == 1111110.
  if ((segs[0]! & 0xfe00) === 0xfc00) return true;
  // Multicast ff00::/8.
  if ((segs[0]! & 0xff00) === 0xff00) return true;
  return false;
}

/**
 * Parse an IPv6 literal into 8 numeric segments. Handles:
 *   - `::` zero-run compression (any position).
 *   - Dotted-decimal tail (`::ffff:127.0.0.1`).
 *   - Plain hex (`2001:db8::1`).
 *
 * Returns `null` for unparseable input. Bracketed form (`[::1]`) is
 * stripped by the caller (`resolveAndCheck`) before reaching here.
 */
export function expandIpv6(addr: string): number[] | null {
  if (typeof addr !== "string" || addr.length === 0) return null;
  const lower = addr.toLowerCase();

  // Split on `::` — at most one occurrence.
  const ccIdx = lower.indexOf("::");
  let head: string[];
  let tail: string[];
  if (ccIdx === -1) {
    head = lower.split(":");
    tail = [];
  } else {
    if (lower.indexOf("::", ccIdx + 1) !== -1) return null;
    const left = lower.slice(0, ccIdx);
    const right = lower.slice(ccIdx + 2);
    head = left ? left.split(":") : [];
    tail = right ? right.split(":") : [];
  }

  // If the last group has a dot, it's an IPv4 dotted-decimal tail
  // — convert to two hex segments.
  const expandDotted = (groups: string[]): boolean => {
    if (groups.length === 0) return true;
    const last = groups[groups.length - 1]!;
    if (!last.includes(".")) return true;
    const octets = last.split(".").map((p) => Number(p));
    if (octets.length !== 4) return false;
    for (const o of octets) {
      if (!Number.isInteger(o) || o < 0 || o > 255) return false;
    }
    groups.pop();
    const hi = (octets[0]! << 8) | octets[1]!;
    const lo = (octets[2]! << 8) | octets[3]!;
    groups.push(hi.toString(16), lo.toString(16));
    return true;
  };
  if (!expandDotted(tail) || !expandDotted(head)) return null;

  let segs: string[];
  if (ccIdx === -1) {
    segs = head;
  } else {
    const fill = 8 - head.length - tail.length;
    if (fill < 0) return null;
    segs = [...head, ...Array(fill).fill("0"), ...tail];
  }
  if (segs.length !== 8) return null;
  const out: number[] = [];
  for (const s of segs) {
    if (s.length === 0 || s.length > 4) return null;
    if (!/^[0-9a-f]+$/.test(s)) return null;
    const n = parseInt(s, 16);
    if (!Number.isInteger(n) || n < 0 || n > 0xffff) return null;
    out.push(n);
  }
  return out;
}

export interface ResolvedHost {
  /** Original hostname / literal as supplied. */
  host: string;
  /** Addresses that survived the SSRF floor. Empty → throws. */
  safe: string[];
  /** Addresses we rejected, for diagnostics. */
  blocked: string[];
}

/**
 * Resolve `host` and return every address that survives the SSRF
 * floor. Throws if every resolved address (or the literal IP) is
 * blocked, or if the hostname is a `localhost` alias.
 */
export async function resolveAndCheck(
  host: string,
  allowLoopback = false,
): Promise<ResolvedHost> {
  // IPv6 in URL form is bracketed (`[::1]`); strip before classification.
  const stripped =
    host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;

  if (net.isIP(stripped)) {
    if (isBlockedIp(stripped, allowLoopback)) {
      throw blockedError(`ip ${stripped} blocked`);
    }
    return { host, safe: [stripped], blocked: [] };
  }

  // WHATWG URL preserves a trailing dot on the hostname, so
  // `new URL("http://localhost./").hostname === "localhost."`. The
  // resolver still treats trailing-dot FQDNs as the same name, so a
  // naive `lc === "localhost"` check would let `localhost.` through
  // the alias gate (the IP-level isBlockedIp would still catch
  // loopback in production, but any single-label internal hostname
  // — `metadata.`, corporate split-horizon names — would slip past).
  // Strip the trailing dot before alias matching.
  const lc = host.toLowerCase().replace(/\.$/, "");
  if (
    !allowLoopback &&
    (lc === "localhost" || lc === "localhost.localdomain" || lc.endsWith(".localhost"))
  ) {
    throw blockedError(`host ${host} blocked`);
  }

  let resolved: dns.LookupAddress[];
  try {
    resolved = await dns.lookup(host, { all: true });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw blockedError(`resolve ${host}: ${msg}`);
  }

  const safe: string[] = [];
  const blocked: string[] = [];
  for (const r of resolved) {
    if (isBlockedIp(r.address, allowLoopback)) blocked.push(r.address);
    else safe.push(r.address);
  }
  if (safe.length === 0) {
    throw blockedError(
      `host ${host} resolved only to blocked IP ranges: ${blocked.join(", ")}`,
    );
  }
  return { host, safe, blocked };
}

/**
 * Pre-navigation URL gate. Throws on non-http(s), unparseable URLs,
 * or hostnames whose every resolved address is blocked.
 */
export async function assertSafeUrl(url: string, allowLoopback = false): Promise<void> {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw blockedError(`invalid url '${url}': ${msg}`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw blockedError(`scheme '${parsed.protocol}' not allowed (use http or https)`);
  }
  if (!parsed.hostname) {
    throw blockedError(`url '${url}' has no host`);
  }
  await resolveAndCheck(parsed.hostname, allowLoopback);
}

function blockedError(message: string): Error {
  const err = new Error(message);
  err.name = "BLOCKED_BY_SSRF_POLICY";
  return err;
}
