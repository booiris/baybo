import { beforeEach, describe, expect, it, vi } from "vitest";
import sdkSource from "./sdkCard.js?raw";

describe("card iframe size injection", () => {
  beforeEach(() => {
    document.documentElement.removeAttribute("data-deck-size");
  });

  it("reflects the initial and live render sizes onto the iframe root", async () => {
    window.eval(sdkSource);
    expect(document.documentElement.dataset.deckSize).toBe("wide");

    const channel = new MessageChannel();
    window.dispatchEvent(
      new MessageEvent("message", {
        data: { type: "deck_init", size: "max" },
        source: window,
        ports: [channel.port2],
      }),
    );
    expect(document.documentElement.dataset.deckSize).toBe("max");

    channel.port1.postMessage({ type: "size", size: "large" });
    // Wait for the EFFECT, never for one timer tick. jsdom implements no
    // MessagePort at all, so `MessageChannel` here is Node's `worker_threads`
    // one and its delivery rides libuv — a task source with no ordering
    // relationship to `setTimeout`. A single tick happens to lose that race
    // often enough to matter on a loaded runner, and the assertion then reads
    // the size the `deck_init` above applied.
    await vi.waitFor(() => {
      expect(document.documentElement.dataset.deckSize).toBe("large");
    });
  });

});
