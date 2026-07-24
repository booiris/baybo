import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InitPayload, TranscriptEvents } from "./bridge";
import type { PersistedState } from "./types";

// `bridge.ts` snapshots `window.webkit` at MODULE SCOPE and installs
// `window.baybo` as a side effect, so every test needs a fresh module instance
// loaded AFTER the WKWebView handler stub is in place.
type Bridge = typeof import("./bridge");

const SESSION_A = "session-a";
const SESSION_B = "session-b";
const PERSIST_DEBOUNCE_MS = 500;

let posted: Record<string, unknown>[];

function initPayload(sessionId: string): InitPayload {
  return { language: "en", sessionId, restoredState: null, connEpoch: 1 };
}

function mirror(text: string): PersistedState {
  return {
    messages: [{ id: "pm-1", role: "user", content: text }],
    lastOrdinal: 7,
    oldestOrdinal: 1,
    hasMoreOlder: false,
  };
}

function persistPosts(): Record<string, unknown>[] {
  return posted.filter((m) => m.type === "persist");
}

async function loadBridge(): Promise<Bridge> {
  vi.resetModules();
  posted = [];
  window.webkit = {
    messageHandlers: {
      baybo: {
        postMessage: (message: unknown) => {
          posted.push(message as Record<string, unknown>);
        },
      },
    },
  };
  return import("./bridge");
}

/// Records every delivered transcript event as `"<kind>:<payload>"`, so a
/// replay's ORDER is assertable as a plain array.
function recorder(): { log: string[]; events: TranscriptEvents } {
  const log: string[] = [];
  return {
    log,
    events: {
      frame: (json) => log.push(`frame:${json}`),
      connEpoch: (epoch) => log.push(`epoch:${epoch}`),
      userSent: (payload) => log.push(`userSent:${payload.msgId}`),
      sendFailed: (msgId) => log.push(`sendFailed:${msgId}`),
      bottomInset: (px) => log.push(`inset:${px}`),
      jumpToLatest: () => log.push("jump"),
      syncRequested: () => log.push("sync"),
      jumpToMessage: (rowId) => log.push(`jumpToMessage:${rowId}`),
      outlineLoadOlder: () => log.push("outlineLoadOlder"),
      outlineHereRequested: () => log.push("outlineHere"),
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  URL.createObjectURL = vi.fn(() => "blob:stub");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("outbound posts", () => {
  it("sees the native bridge once the WKWebView handler is installed", async () => {
    const bridge = await loadBridge();
    expect(bridge.hasNativeBridge).toBe(true);
  });

  // `init` SEEDS the epoch from its payload — it is not left at 0 until the next
  // `setConnEpoch`. The transcript keys image retries and the reconnect sync off
  // this value, so a retarget that kept a stale epoch would fire a spurious
  // "reconnected" pass on the newly-mounted tree.
  it("adopts the connEpoch init carries, not a zero default", async () => {
    const bridge = await loadBridge();
    window.baybo.init({ ...initPayload(SESSION_A), connEpoch: 7 });
    expect(bridge.currentConnEpoch()).toBe(7);
  });

  it("posts ready / sync / runState in the shapes native parses", async () => {
    const bridge = await loadBridge();
    bridge.postReady();
    bridge.postSyncRequest(null, 50);
    bridge.postRunState(true);
    expect(posted).toEqual([
      { type: "ready" },
      { type: "sync", sinceOrdinal: null, limit: 50 },
      { type: "runState", running: true },
    ]);
  });
});

describe("persistState — the cross-session mirror guard", () => {
  it("writes nothing before init: there is no session to attribute the rows to", async () => {
    const bridge = await loadBridge();
    bridge.persistState(mirror("orphan"));
    vi.advanceTimersByTime(PERSIST_DEBOUNCE_MS * 4);
    expect(persistPosts()).toEqual([]);
  });

  it("debounces a catch-up burst into ONE write, carrying the last state", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.persistState(mirror("one"));
    bridge.persistState(mirror("two"));
    bridge.persistState(mirror("three"));
    expect(persistPosts()).toEqual([]);

    vi.advanceTimersByTime(PERSIST_DEBOUNCE_MS);
    expect(persistPosts()).toEqual([
      { type: "persist", sessionId: SESSION_A, state: mirror("three") },
    ]);
  });

  // NOTE — do not "restore" a test here claiming to pin `persistState`'s
  // CALL-TIME capture of the sessionId. That capture is real (bridge.ts) but it
  // is UNOBSERVABLE from outside: `init` is the only mutator of the live session
  // and it also drops the pending write (next test), so no reachable state has a
  // pending write outliving its session. A test that inits A, persists, and
  // flushes passes identically whether the tag is read at call time or at flush
  // time — it pins nothing. The test below is the real guard.
  it("DROPS session A's pending debounced write on init(B) — A's rows must never land in B's mirror", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.persistState(mirror("a-rows"));

    window.baybo.init(initPayload(SESSION_B));
    vi.advanceTimersByTime(PERSIST_DEBOUNCE_MS * 4);

    expect(persistPosts()).toEqual([]);
  });

  it("writes B's own rows under B after the retarget", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.persistState(mirror("a-rows"));
    window.baybo.init(initPayload(SESSION_B));
    bridge.persistState(mirror("b-rows"));
    vi.advanceTimersByTime(PERSIST_DEBOUNCE_MS);

    expect(persistPosts()).toEqual([
      { type: "persist", sessionId: SESSION_B, state: mirror("b-rows") },
    ]);
  });

  it("flushPersist writes the pending mirror immediately (native calls it before detaching)", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.persistState(mirror("live steps"));

    window.baybo.flushPersist();
    expect(persistPosts()).toEqual([
      { type: "persist", sessionId: SESSION_A, state: mirror("live steps") },
    ]);

    // The debounce timer it pre-empted must not fire a second write, and a
    // flush with nothing pending is a no-op.
    vi.advanceTimersByTime(PERSIST_DEBOUNCE_MS * 4);
    window.baybo.flushPersist();
    expect(persistPosts()).toHaveLength(1);
  });
});

describe("postOutline — the message index mirror", () => {
  function outline(text: string): Parameters<Bridge["postOutline"]>[0] {
    return {
      entries: [
        { id: "pm-1", text, gloss: "done", at: "09:12", dayKey: "2026-07-23", attachments: 0 },
      ],
      hasMoreOlder: true,
      loadingOlder: false,
      available: true,
    };
  }

  function outlinePosts(): Record<string, unknown>[] {
    return posted.filter((m) => m.type === "outline");
  }

  // The header button's presence is driven by this payload, so the FIRST one
  // must not wait out a debounce — the control would flicker in a beat after
  // the screen settles.
  it("posts the first outline immediately, flat, in the shape native parses", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.postOutline(outline("hi"));
    expect(outlinePosts()).toEqual([
      {
        type: "outline",
        entries: [
          { id: "pm-1", text: "hi", gloss: "done", at: "09:12", dayKey: "2026-07-23", attachments: 0 },
        ],
        hasMoreOlder: true,
        loadingOlder: false,
        available: true,
      },
    ]);
  });

  it("drops a byte-identical repost — the transcript re-derives on every commit", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.postOutline(outline("hi"));
    bridge.postOutline(outline("hi"));
    vi.advanceTimersByTime(1000);
    expect(outlinePosts()).toHaveLength(1);
  });

  it("trailing-debounces every post after the first, keeping the LAST state", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.postOutline(outline("first"));
    bridge.postOutline(outline("second"));
    bridge.postOutline(outline("third"));
    expect(outlinePosts()).toHaveLength(1);

    vi.advanceTimersByTime(250);
    const posts = outlinePosts();
    expect(posts).toHaveLength(2);
    expect((posts[1].entries as { text: string }[])[0].text).toBe("third");
  });

  // One WKWebView serves every conversation: without the reset, session B's
  // first outline is compared against session A's and — if the two happen to
  // match — never posts at all, leaving native holding A's rows.
  it("re-posts immediately after init even when the payload is identical", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));
    bridge.postOutline(outline("same"));
    expect(outlinePosts()).toHaveLength(1);

    window.baybo.init(initPayload(SESSION_B));
    bridge.postOutline(outline("same"));
    expect(outlinePosts()).toHaveLength(2);
  });

  it("posts the reader's position as an uncorrelated answer, null when nothing is above the fold", async () => {
    const bridge = await loadBridge();
    bridge.postOutlineHere("pm-9");
    bridge.postOutlineHere(null);
    expect(posted.filter((m) => m.type === "outlineHere")).toEqual([
      { type: "outlineHere", rowId: "pm-9" },
      { type: "outlineHere", rowId: null },
    ]);
  });
});

describe("the pre-subscribe buffer", () => {
  it("replays everything native pushed before the transcript subscribed, in ARRIVAL ORDER", async () => {
    const bridge = await loadBridge();
    window.baybo.init(initPayload(SESSION_A));

    window.baybo.pushFrame("frame-1");
    window.baybo.setConnEpoch(2);
    window.baybo.userSent({ msgId: "pm-1", text: "hi", attachments: [] });
    window.baybo.sendFailed("pm-1");
    window.baybo.setBottomInset(120);
    window.baybo.jumpToLatest();
    window.baybo.requestSync();
    window.baybo.pushFrame("frame-2");

    const { log, events } = recorder();
    expect(log).toEqual([]);

    bridge.subscribeTranscript(events);
    expect(log).toEqual([
      "frame:frame-1",
      "epoch:2",
      "userSent:pm-1",
      "sendFailed:pm-1",
      "inset:120",
      "jump",
      "sync",
      "frame:frame-2",
    ]);
    expect(bridge.currentConnEpoch()).toBe(2);
  });

  it("delivers straight through once subscribed, and drains the buffer exactly once", async () => {
    const bridge = await loadBridge();
    window.baybo.pushFrame("buffered");

    const first = recorder();
    bridge.subscribeTranscript(first.events);
    window.baybo.pushFrame("live");
    expect(first.log).toEqual(["frame:buffered", "frame:live"]);

    const second = recorder();
    bridge.subscribeTranscript(second.events);
    expect(second.log).toEqual([]);
  });

  it("CLEARS the buffer on init — nothing from the old session leaks into the new tree", async () => {
    const bridge = await loadBridge();
    window.baybo.pushFrame("session-a-frame");

    window.baybo.init(initPayload(SESSION_B));

    const { log, events } = recorder();
    bridge.subscribeTranscript(events);
    expect(log).toEqual([]);
  });

  // `deliver()` ends in a BARE `else e.jumpToLatest()`, so a command whose
  // `Buffered` variant has no explicit branch silently becomes "scroll to the
  // bottom" — and TypeScript cannot catch it (the union is exhausted by the
  // else). This is the one guard against shipping the index's commands as a
  // jump-to-latest.
  it("replays each index command as ITSELF, never as the jumpToLatest fall-through", async () => {
    const bridge = await loadBridge();
    window.baybo.jumpToMessage("m42");
    window.baybo.outlineLoadOlder();
    window.baybo.requestOutlineHere();

    const { log, events } = recorder();
    bridge.subscribeTranscript(events);
    expect(log).toEqual(["jumpToMessage:m42", "outlineLoadOlder", "outlineHere"]);
    expect(log).not.toContain("jump");
  });

  it("re-buffers after unsubscribe, so a detached window loses nothing", async () => {
    const bridge = await loadBridge();
    const first = recorder();
    const off = bridge.subscribeTranscript(first.events);
    off();

    window.baybo.pushFrame("while-detached");
    expect(first.log).toEqual([]);

    const second = recorder();
    bridge.subscribeTranscript(second.events);
    expect(second.log).toEqual(["frame:while-detached"]);
  });
});

describe("per-blob fan-out", () => {
  it("delivers a fileState tick only to the cards on THAT blob", async () => {
    const bridge = await loadBridge();
    const a1 = vi.fn();
    const a2 = vi.fn();
    const b = vi.fn();
    bridge.onFileState("blob-a", a1);
    bridge.onFileState("blob-a", a2);
    bridge.onFileState("blob-b", b);

    window.baybo.fileState({ blobId: "blob-a", state: "loading", loaded: 512, total: 2048 });

    expect(a1).toHaveBeenCalledWith({ blobId: "blob-a", state: "loading", loaded: 512, total: 2048 });
    expect(a2).toHaveBeenCalledTimes(1);
    expect(b).not.toHaveBeenCalled();
  });

  it("stops delivering after unsubscribe, and a tick for a blob no card is showing is a no-op", async () => {
    const bridge = await loadBridge();
    const listener = vi.fn();
    const off = bridge.onFileState("blob-a", listener);
    off();

    window.baybo.fileState({ blobId: "blob-a", state: "ready" });
    expect(() => window.baybo.fileState({ blobId: "never-seen", state: "ready" })).not.toThrow();
    expect(listener).not.toHaveBeenCalled();
  });

  it("fans audio ticks out per blob and unsubscribes cleanly", async () => {
    const bridge = await loadBridge();
    const track = vi.fn();
    const other = vi.fn();
    const off = bridge.onAudioState("blob-a", track);
    bridge.onAudioState("blob-b", other);

    window.baybo.audioState({ blobId: "blob-a", state: "playing", position: 3, duration: 200 });
    expect(track).toHaveBeenCalledWith({ blobId: "blob-a", state: "playing", position: 3, duration: 200 });
    expect(other).not.toHaveBeenCalled();

    off();
    window.baybo.audioState({ blobId: "blob-a", state: "paused", position: 4, duration: 200 });
    expect(track).toHaveBeenCalledTimes(1);
  });
});

describe("blob requests", () => {
  it("settles the pending request the blobResult names by id", async () => {
    const bridge = await loadBridge();
    const pending = bridge.blobObjectUrl("sha256:ab.tok", "image/png");
    const request = posted.find((m) => m.type === "requestBlob");
    expect(request).toMatchObject({ type: "requestBlob", blobId: "sha256:ab.tok" });

    window.baybo.blobResult({
      id: request?.id as number,
      dataBase64: btoa("bytes"),
      mimeType: "image/png",
      error: null,
    });

    await expect(pending).resolves.toBe("blob:stub");
  });

  // A thread's images fetch CONCURRENTLY (the lazy IntersectionObserver fires a
  // whole screenful at once), so settling "the pending request" instead of "the
  // one this id names" would hand image B's bytes to image A's bubble. One
  // request in flight cannot tell those apart — this needs two.
  it("settles ONLY the request the id names, leaving the other in flight", async () => {
    const bridge = await loadBridge();
    const first = bridge.blobObjectUrl("sha256:aa.tok", "image/png");
    const second = bridge.blobObjectUrl("sha256:bb.tok", "image/jpeg");
    const [reqA, reqB] = posted.filter((m) => m.type === "requestBlob");
    expect(reqA.id).not.toBe(reqB.id);

    let firstSettled = false;
    void first.then(
      () => {
        firstSettled = true;
      },
      () => {
        firstSettled = true;
      },
    );

    window.baybo.blobResult({
      id: reqB.id as number,
      dataBase64: btoa("b-bytes"),
      mimeType: "image/jpeg",
      error: null,
    });
    await expect(second).resolves.toBe("blob:stub");

    await vi.advanceTimersByTimeAsync(0);
    expect(firstSettled).toBe(false);
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);

    // ...and the still-open request settles with its OWN bytes when its id lands.
    window.baybo.blobResult({
      id: reqA.id as number,
      dataBase64: btoa("a-bytes"),
      mimeType: "image/png",
      error: null,
    });
    await expect(first).resolves.toBe("blob:stub");
  });

  it("rejects when native reports the fetch failed", async () => {
    const bridge = await loadBridge();
    const pending = bridge.blobObjectUrl("sha256:ab.tok", "image/png");
    const request = posted.find((m) => m.type === "requestBlob");

    window.baybo.blobResult({
      id: request?.id as number,
      dataBase64: null,
      mimeType: "",
      error: "blob purged",
    });

    await expect(pending).rejects.toThrow("blob purged");
  });

  it("CLEARS blobPending on init, so a late result from the old session settles nothing", async () => {
    const bridge = await loadBridge();
    let settled = false;
    const pending = bridge.blobObjectUrl("sha256:ab.tok", "image/png");
    void pending.then(
      () => {
        settled = true;
      },
      () => {
        settled = true;
      },
    );
    const request = posted.find((m) => m.type === "requestBlob");

    window.baybo.init(initPayload(SESSION_B));
    window.baybo.blobResult({
      id: request?.id as number,
      dataBase64: btoa("bytes"),
      mimeType: "image/png",
      error: null,
    });
    await vi.advanceTimersByTimeAsync(0);

    expect(settled).toBe(false);
    expect(URL.createObjectURL).not.toHaveBeenCalled();
  });
});
