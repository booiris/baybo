import { describe, expect, it } from "vitest";
import {
  CARD_CSP,
  DeckCard,
  applyCardData,
  buildSrcdoc,
  cycleSize,
  layoutEntries,
  reorderTo,
  parsePayload,
  replaceState,
} from "./state";

function card(id: string, position: number, size: DeckCard["size"] = "wide"): DeckCard {
  return {
    cardId: id,
    title: id,
    position,
    size,
    enabled: true,
    quarantined: false,
    specHash: "h",
    lastSeq: 0,
  };
}

describe("replaceState", () => {
  it("orders cards by position and replaces snapshots unconditionally", () => {
    const prev = replaceState([card("a", 0)], [
      { cardId: "a", seq: 9, payload: `{"n":9}` },
    ]);
    expect(prev.snaps.a?.seq).toBe(9);

    // A DeckChanged refetch replaces snapshot + seq even if LOWER — the
    // refetch is the authority (the sentinel/cursor bug class).
    const next = replaceState(
      [card("b", 0), card("a", 1)],
      [{ cardId: "a", seq: 3, payload: `{"n":3}` }],
    );
    expect(next.cards.map((c) => c.cardId)).toEqual(["b", "a"]);
    expect(next.snaps.a?.seq).toBe(3);
  });
});

describe("applyCardData", () => {
  const base = replaceState([card("a", 0)], [
    { cardId: "a", seq: 5, payload: `{"n":5}` },
  ]);

  it("accepts iff seq is strictly greater than cached", () => {
    expect(applyCardData(base, "a", 5, "{}").changed).toBe(false);
    expect(applyCardData(base, "a", 4, "{}").changed).toBe(false);
    const up = applyCardData(base, "a", 6, `{"n":6}`);
    expect(up.changed).toBe(true);
    expect(up.state.snaps.a?.seq).toBe(6);
  });

  it("accepts a first push for a card with no cached snapshot", () => {
    const empty = replaceState([card("a", 0)], []);
    expect(applyCardData(empty, "a", 1, "{}").changed).toBe(true);
  });

  it("drops pushes for unknown cards", () => {
    expect(applyCardData(base, "ghost", 99, "{}").changed).toBe(false);
  });
});

describe("layout math", () => {
  const cards = [card("a", 0), card("b", 1), card("c", 2)];

  it("adopts the settled DOM order and renumbers", () => {
    const moved = reorderTo(cards, ["c", "a", "b"]);
    expect(moved.map((c) => c.cardId)).toEqual(["c", "a", "b"]);
    expect(moved.map((c) => c.position)).toEqual([0, 1, 2]);
  });

  it("ignores unknown ids and keeps missing cards at the tail", () => {
    const moved = reorderTo(cards, ["ghost", "b"]);
    expect(moved.map((c) => c.cardId)).toEqual(["b", "a", "c"]);
    expect(moved.map((c) => c.position)).toEqual([0, 1, 2]);
  });

  it("emits layout entries in flow order", () => {
    const entries = layoutEntries(reorderTo(cards, ["b", "c", "a"]));
    expect(entries).toEqual([
      { cardId: "b", position: 0, size: "wide" },
      { cardId: "c", position: 1, size: "wide" },
      { cardId: "a", position: 2, size: "wide" },
    ]);
  });

  it("cycles sizes small → wide → large → small", () => {
    expect(cycleSize("small")).toBe("wide");
    expect(cycleSize("wide")).toBe("large");
    expect(cycleSize("large")).toBe("small");
  });
});

describe("srcdoc composition", () => {
  it("orders CSP, then base style, then SDK, then the card fragment", () => {
    const doc = buildSrcdoc("<div id=card></div>", "/*sdk*/", "/*base*/");
    expect(doc).toContain(CARD_CSP);
    const cspAt = doc.indexOf("Content-Security-Policy");
    const baseAt = doc.indexOf("/*base*/");
    const sdkAt = doc.indexOf("/*sdk*/");
    const cardAt = doc.indexOf("<div id=card>");
    expect(cspAt).toBeGreaterThan(-1);
    // Base style precedes the fragment so a card's own <style> wins the
    // cascade — that ordering IS the override contract.
    expect(baseAt).toBeGreaterThan(cspAt);
    expect(sdkAt).toBeGreaterThan(baseAt);
    expect(cardAt).toBeGreaterThan(sdkAt);
    // No network is reachable under this CSP even though sandbox alone
    // would allow a fire-and-forget fetch.
    expect(CARD_CSP).toContain("default-src 'none'");
  });
});

describe("parsePayload", () => {
  it("parses JSON payloads and degrades on errors", () => {
    expect(parsePayload({ seq: 1, payload: `{"a":1}`, error: null })).toEqual({ a: 1 });
    expect(parsePayload({ seq: 1, payload: "", error: "boom" })).toEqual({ error: "boom" });
    expect(parsePayload({ seq: 1, payload: "not json", error: null })).toBeNull();
  });
});
