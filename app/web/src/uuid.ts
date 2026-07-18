// `crypto.randomUUID()` exists ONLY in a secure context (https:// or
// localhost). The dashboard is routinely opened over plain http:// on a LAN IP,
// where it is undefined and a call throws `crypto.randomUUID is not a function`
// — which wedged the composer on first input. `crypto.getRandomValues()` is NOT
// gated to secure contexts, so mint a v4 UUID from it there, and fall back to
// Math.random only if Web Crypto is entirely absent. These ids key
// optimistic-send reconciliation (clientMsgId / outbox), so they must stay
// collision-free — a real UUID, not a counter.
export function uuid(): string {
  // The DOM lib types these as always-present, but `randomUUID` (and Web Crypto
  // as a whole) is absent over insecure http://, so treat them as runtime-optional.
  const c = globalThis.crypto as
    | { randomUUID?: () => string; getRandomValues?: (array: Uint8Array) => Uint8Array }
    | undefined;

  if (typeof c?.randomUUID === "function") return c.randomUUID();

  const bytes = new Uint8Array(16);
  if (typeof c?.getRandomValues === "function") {
    c.getRandomValues(bytes);
  } else {
    for (let i = 0; i < 16; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  }
  // RFC 4122 v4: pin the version (4) and variant (10xx) bits.
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;

  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex.slice(6, 8).join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10, 16).join("")}`;
}
