import { beforeEach, describe, expect, it } from "vitest";
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
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(document.documentElement.dataset.deckSize).toBe("large");
  });

});
