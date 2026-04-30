// Unit tests for the SSRF policy port. Run with `pnpm test` from
// inside `tool-src/browser/`. The bundle is built so we test the
// compiled JS — but at this layer there's no need: load the source
// `.ts` directly via Node's experimental TS support? We don't have
// that, so we re-implement the small relevant surface here against
// the bundled output.
//
// Approach: import the bundled file (which exposes nothing at
// runtime — it's an entry point) is wrong. Instead, the source TS
// is compiled to ESM at bundle time. Here we just reach into the
// raw module by importing `dist/bundle.mjs` won't expose
// `isBlockedIp`. So the cleanest path: copy-paste the same tests
// over a tiny harness that imports a separate ESM entry that
// re-exports the needed surface.
//
// Simpler practical answer: the network_policy module is pure JS
// after compilation; we use `tsx` (not installed) or compile-once
// via a small esbuild call. Easiest: bundle network_policy alone
// to a test-only ESM file at `dist/network_policy_test.mjs` and
// import it.
//
// This test file imports the compiled output produced by the
// `bundle:test` npm script (defined alongside `bundle`). Run via
// `pnpm test` which depends on `bundle:test`.

import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  expandIpv6,
  isBlockedIp,
  resolveAndCheck,
} from "../dist/network_policy_test.mjs";

// ----------------------------------------------------------------
// expandIpv6: IPv6 literal parsing edge cases
// ----------------------------------------------------------------

test("expandIpv6 handles dotted IPv4-mapped form", () => {
  assert.deepEqual(expandIpv6("::ffff:127.0.0.1"), [
    0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001,
  ]);
});

test("expandIpv6 handles canonical hex form WHATWG URL produces", () => {
  // Node's `new URL('http://[::ffff:127.0.0.1]/').hostname` returns
  // `::ffff:7f00:1` — the bug the adversarial review flagged.
  assert.deepEqual(expandIpv6("::ffff:7f00:1"), [
    0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001,
  ]);
});

test("expandIpv6 fills :: in the middle", () => {
  assert.deepEqual(expandIpv6("2001:db8::1"), [
    0x2001, 0x0db8, 0, 0, 0, 0, 0, 0x0001,
  ]);
});

test("expandIpv6 handles full-form addresses", () => {
  assert.deepEqual(expandIpv6("2001:0db8:0000:0000:0000:0000:0000:0001"), [
    0x2001, 0x0db8, 0, 0, 0, 0, 0, 1,
  ]);
});

test("expandIpv6 rejects double ::", () => {
  assert.equal(expandIpv6("1::2::3"), null);
});

test("expandIpv6 rejects too many groups", () => {
  assert.equal(expandIpv6("1:2:3:4:5:6:7:8:9"), null);
});

test("expandIpv6 rejects non-hex characters", () => {
  assert.equal(expandIpv6("zzzz::1"), null);
});

// ----------------------------------------------------------------
// isBlockedIp: SSRF deny set
// ----------------------------------------------------------------

test("isBlockedIp blocks IPv4 loopback in strict mode", () => {
  assert.equal(isBlockedIp("127.0.0.1"), true);
  assert.equal(isBlockedIp("127.255.255.254"), true);
});

test("isBlockedIp permits IPv4 loopback when allowLoopback=true", () => {
  assert.equal(isBlockedIp("127.0.0.1", true), false);
});

test("isBlockedIp blocks RFC1918 v4", () => {
  assert.equal(isBlockedIp("10.0.0.1"), true);
  assert.equal(isBlockedIp("172.16.0.1"), true);
  assert.equal(isBlockedIp("192.168.1.1"), true);
});

test("isBlockedIp blocks v4 link-local incl. metadata IP", () => {
  assert.equal(isBlockedIp("169.254.169.254"), true);
});

test("isBlockedIp blocks IPv6 loopback ::1", () => {
  assert.equal(isBlockedIp("::1"), true);
});

test("isBlockedIp blocks ::ffff:127.0.0.1 (dotted form)", () => {
  assert.equal(isBlockedIp("::ffff:127.0.0.1"), true);
});

test("isBlockedIp blocks ::ffff:7f00:1 (hex form WHATWG canonicalises to)", () => {
  // The bug: regex-based parsing missed this. Now fixed.
  assert.equal(isBlockedIp("::ffff:7f00:1"), true);
});

test("isBlockedIp blocks ::ffff:7f00:0001 (zero-padded hex form)", () => {
  assert.equal(isBlockedIp("::ffff:7f00:0001"), true);
});

test("isBlockedIp blocks ::ffff:a9fe:a9fe (hex 169.254.169.254 metadata)", () => {
  assert.equal(isBlockedIp("::ffff:a9fe:a9fe"), true);
});

test("isBlockedIp blocks ::ffff:0a00:1 (hex 10.0.0.1 RFC1918)", () => {
  assert.equal(isBlockedIp("::ffff:0a00:1"), true);
});

test("isBlockedIp blocks ::ffff:c0a8:101 (hex 192.168.1.1 RFC1918)", () => {
  assert.equal(isBlockedIp("::ffff:c0a8:101"), true);
});

test("isBlockedIp blocks IPv6 link-local fe80::/10", () => {
  assert.equal(isBlockedIp("fe80::1"), true);
  assert.equal(isBlockedIp("febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff"), true);
});

test("isBlockedIp blocks IPv6 unique-local fc00::/7", () => {
  assert.equal(isBlockedIp("fc00::1"), true);
  assert.equal(isBlockedIp("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"), true);
});

test("isBlockedIp blocks IPv6 multicast ff00::/8", () => {
  assert.equal(isBlockedIp("ff02::1"), true);
});

test("isBlockedIp blocks IPv6 unspecified ::", () => {
  assert.equal(isBlockedIp("::"), true);
});

test("isBlockedIp permits public IPv4", () => {
  assert.equal(isBlockedIp("1.1.1.1"), false);
  assert.equal(isBlockedIp("8.8.8.8"), false);
});

test("isBlockedIp permits public IPv6", () => {
  assert.equal(isBlockedIp("2606:4700:4700::1111"), false);
  assert.equal(isBlockedIp("2001:db8::1"), false);
});

test("isBlockedIp blocks broadcast / unspecified v4", () => {
  assert.equal(isBlockedIp("0.0.0.0"), true);
  assert.equal(isBlockedIp("255.255.255.255"), true);
});

test("isBlockedIp blocks v4 documentation ranges", () => {
  assert.equal(isBlockedIp("192.0.2.1"), true);
  assert.equal(isBlockedIp("198.51.100.1"), true);
  assert.equal(isBlockedIp("203.0.113.1"), true);
});

test("isBlockedIp blocks CGNAT 100.64/10", () => {
  assert.equal(isBlockedIp("100.64.0.1"), true);
  assert.equal(isBlockedIp("100.127.255.254"), true);
});

test("isBlockedIp blocks reserved 240/4", () => {
  assert.equal(isBlockedIp("240.0.0.1"), true);
});

test("isBlockedIp blocks unrecognisable input", () => {
  assert.equal(isBlockedIp("not-an-ip"), true);
});

// ----------------------------------------------------------------
// resolveAndCheck: literal IP + localhost alias
// ----------------------------------------------------------------

test("resolveAndCheck blocks literal RFC1918 IP", async () => {
  await assert.rejects(() => resolveAndCheck("10.0.0.1"));
});

test("resolveAndCheck blocks bracketed IPv6 literal", async () => {
  await assert.rejects(() => resolveAndCheck("[::1]"));
});

test("resolveAndCheck blocks ::ffff:7f00:1 hex IPv4-mapped", async () => {
  await assert.rejects(() => resolveAndCheck("::ffff:7f00:1"));
});

test("resolveAndCheck blocks localhost alias", async () => {
  await assert.rejects(() => resolveAndCheck("localhost"));
  await assert.rejects(() => resolveAndCheck("foo.localhost"));
});

test("resolveAndCheck blocks trailing-dot localhost FQDNs", async () => {
  // WHATWG URL preserves the trailing dot on hostname:
  //   new URL("http://localhost./").hostname === "localhost."
  // The pre-fix alias check did `lc === "localhost"` which let the
  // trailing-dot form through to dns.lookup. IP-level check still
  // catches loopback in production, but any single-label internal
  // hostname (`metadata.`, corporate split-horizon names) would slip.
  await assert.rejects(() => resolveAndCheck("localhost."));
  await assert.rejects(() => resolveAndCheck("foo.localhost."));
  await assert.rejects(() => resolveAndCheck("localhost.localdomain."));
});

test("resolveAndCheck permits localhost alias when allowLoopback=true", async () => {
  const r = await resolveAndCheck("localhost", true);
  assert.ok(r.safe.length > 0, "expected at least one safe address");
});
