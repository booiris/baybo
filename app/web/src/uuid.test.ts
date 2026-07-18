import { afterEach, describe, expect, it, vi } from "vitest";

import { uuid } from "./uuid";

// Guards the insecure-context fallback: `crypto.randomUUID` is undefined over
// plain http:// on a LAN IP, and a bare call threw `crypto.randomUUID is not a
// function`, wedging the composer. uuid() must still mint a well-formed, unique
// v4 there (via getRandomValues), and prefer the native one when present.

const V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("uuid", () => {
  it("uses crypto.randomUUID when the context is secure", () => {
    const native = vi.fn(() => "11111111-2222-4333-8444-555555555555");
    vi.stubGlobal("crypto", { randomUUID: native, getRandomValues: () => undefined });
    expect(uuid()).toBe("11111111-2222-4333-8444-555555555555");
    expect(native).toHaveBeenCalledOnce();
  });

  it("falls back to getRandomValues when randomUUID is missing (insecure context)", () => {
    // Model an http:// LAN origin: no randomUUID, but getRandomValues works.
    vi.stubGlobal("crypto", {
      getRandomValues: (a: Uint8Array) => {
        for (let i = 0; i < a.length; i += 1) a[i] = (i * 37 + 11) & 0xff;
        return a;
      },
    });
    const id = uuid();
    expect(id).toMatch(V4);
  });

  it("still mints a valid v4 with no Web Crypto at all", () => {
    vi.stubGlobal("crypto", undefined);
    expect(uuid()).toMatch(V4);
  });

  it("does not collide across many mints", () => {
    vi.stubGlobal("crypto", {
      getRandomValues: (a: Uint8Array) => {
        for (let i = 0; i < a.length; i += 1) a[i] = Math.floor(Math.random() * 256);
        return a;
      },
    });
    const ids = new Set(Array.from({ length: 1000 }, () => uuid()));
    expect(ids.size).toBe(1000);
  });
});
