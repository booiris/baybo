import { afterEach, describe, expect, it } from "vitest";

import { hostOrigin, nativeChannel } from "./transport";

afterEach(() => {
  delete window.webkit;
  delete window.bayboHost;
  delete window.deckHost;
});

describe("nativeChannel", () => {
  it("hands iOS the object and Android the same object as a string", () => {
    const webkitPosts: unknown[] = [];
    window.webkit = {
      messageHandlers: {
        baybo: { postMessage: (m: unknown) => void webkitPosts.push(m) },
      },
    };
    const message = { type: "ready", targetId: "s-1" };
    nativeChannel("baybo").post(message);
    expect(webkitPosts).toEqual([message]);

    delete window.webkit;
    const androidPosts: string[] = [];
    window.bayboHost = { postMessage: (m: string) => void androidPosts.push(m) };
    nativeChannel("baybo").post(message);
    // Serialized here, not at the call site: a WebMessageListener takes a
    // string, and the shapes must stay identical across the two hosts.
    expect(androidPosts.map((raw) => JSON.parse(raw))).toEqual([message]);
  });

  /// The whole existing suite stubs `window.webkit`. If Android ever won a tie,
  /// those stubs would stop receiving and the failures would look unrelated.
  it("prefers iOS when both hosts are present", () => {
    const webkitPosts: unknown[] = [];
    const androidPosts: string[] = [];
    window.webkit = {
      messageHandlers: {
        baybo: { postMessage: (m: unknown) => void webkitPosts.push(m) },
      },
    };
    window.bayboHost = { postMessage: (m: string) => void androidPosts.push(m) };
    nativeChannel("baybo").post({ type: "ready" });
    expect(webkitPosts).toHaveLength(1);
    expect(androidPosts).toHaveLength(0);
  });

  it("routes the deck channel to its own host", () => {
    const deckPosts: string[] = [];
    window.deckHost = { postMessage: (m: string) => void deckPosts.push(m) };
    nativeChannel("deck").post({ type: "deck_ready" });
    nativeChannel("baybo").post({ type: "ready" });
    expect(deckPosts.map((raw) => JSON.parse(raw))).toEqual([
      { type: "deck_ready" },
    ]);
  });

  /// A plain dev browser has neither host. The page has to keep running, and
  /// callers read `available` to decide whether to offer a native-only action.
  it("reports unavailable and does not throw with no host", () => {
    const channel = nativeChannel("baybo");
    expect(channel.available).toBe(false);
    expect(() => channel.post({ type: "ready" })).not.toThrow();
  });
});

describe("hostOrigin", () => {
  /// Every URL the bundle builds is root-relative and resolves against this.
  /// It is spelled out only for the deck card's CSP, whose sandboxed frame has
  /// an opaque origin and therefore cannot say `'self'`.
  it("is the document's scheme and host", () => {
    expect(hostOrigin()).toBe(
      `${window.location.protocol}//${window.location.host}`,
    );
  });
});
