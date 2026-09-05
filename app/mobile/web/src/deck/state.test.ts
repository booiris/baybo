import { describe, expect, it } from "vitest";
import {
  cardCsp,
  DeckCard,
  PendingRegistry,
  applyCardData,
  buildSrcdoc,
  cycleSize,
  layoutEntries,
  normalizeAccept,
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
    sizes: [size],
    maximize: false,
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

  it("cycles within the card's declared size set, in canonical order", () => {
    const all: DeckCard["size"][] = ["small", "wide", "large"];
    expect(cycleSize("small", all)).toBe("wide");
    expect(cycleSize("wide", all)).toBe("large");
    expect(cycleSize("large", all)).toBe("small");

    // A two-size card skips the size it doesn't implement.
    expect(cycleSize("wide", ["wide", "large"])).toBe("large");
    expect(cycleSize("large", ["wide", "large"])).toBe("wide");

    // A single-size card has nothing to cycle to.
    expect(cycleSize("large", ["large"])).toBe("large");
  });
});

describe("normalizeCard", () => {
  it("defaults sizes to [size] and maximize to false for a legacy card", () => {
    const c = replaceState([card("a", 0)], []).cards[0];
    expect(c.sizes).toEqual(["wide"]);
    expect(c.maximize).toBe(false);
  });

  it("keeps a declared set in canonical order and guarantees size ∈ sizes", () => {
    const raw = {
      ...card("a", 0, "small"),
      sizes: ["large", "wide"] as DeckCard["size"][],
      maximize: true,
    };
    const c = replaceState([raw], []).cards[0];
    // `small` (the current size) is folded in; order is canonical.
    expect(c.sizes).toEqual(["small", "wide", "large"]);
    expect(c.maximize).toBe(true);
  });
});

describe("srcdoc composition", () => {
  it("orders CSP, then base style, then SDK, then the card fragment", () => {
    const origin = "baybo-transcript://localhost";
    const doc = buildSrcdoc("<div id=card></div>", "/*sdk*/", "/*base*/", origin);
    expect(doc).toContain(cardCsp(origin));
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
    expect(cardCsp(origin)).toContain("default-src 'none'");
    // The card frame is opaque-origin, so `'self'` would match nothing: the
    // host's real origin is the only thing that lets a card show a blob.
    expect(cardCsp(origin)).toContain(`img-src data: ${origin};`);
    expect(cardCsp("https://appassets.androidplatform.net")).toContain(
      "img-src data: https://appassets.androidplatform.net;",
    );
  });
});

describe("normalizeAccept", () => {
  it("takes an array or a comma-separated string and canonicalizes both", () => {
    expect(normalizeAccept(["image/*", "application/pdf"])).toBe("image/*,application/pdf");
    // The pre-array authoring form keeps working.
    expect(normalizeAccept("image/*,application/pdf")).toBe("image/*,application/pdf");
    expect(normalizeAccept(" IMAGE/PNG , image/png ")).toBe("image/png");
    expect(normalizeAccept("*/*")).toBe("*/*");
  });

  it("degrades anything unusable to null — the no-constraint (photo) floor", () => {
    expect(normalizeAccept(undefined)).toBeNull();
    expect(normalizeAccept(null)).toBeNull();
    expect(normalizeAccept("")).toBeNull();
    expect(normalizeAccept("nonsense")).toBeNull();
    expect(normalizeAccept(["/", "x/", "/y", ""])).toBeNull();
    expect(normalizeAccept({ accept: "image/*" })).toBeNull();
    // A garbage member is dropped; the good ones survive.
    expect(normalizeAccept(["image/*", "nonsense", 7])).toBe("image/*");
  });
});

describe("PendingRegistry", () => {
  const port = () => new MessageChannel().port1;

  it("delivers a result only to the generation that asked", () => {
    const registry = new PendingRegistry();
    const live = port();
    const id = registry.add("a", 1, live, "pick");
    expect(registry.claim(id, () => live)?.localId).toBe(1);
    expect(registry.size).toBe(0);
  });

  it("drops a result addressed to a reloaded or removed generation", () => {
    const registry = new PendingRegistry();
    const stale = port();
    // A spec-change reload gives the tile a NEW port while the pick is open;
    // the late answer must not resolve the new generation's promise.
    const reloaded = registry.add("a", 1, stale, "pick");
    expect(registry.claim(reloaded, () => port())).toBeNull();
    // Tile gone entirely (removed card).
    const removed = registry.add("a", 2, stale, "pick");
    expect(registry.claim(removed, () => null)).toBeNull();
    // Either way the entry is consumed — the map can't accumulate.
    expect(registry.size).toBe(0);
  });

  it("mints distinct ids per card and purges a card's pending requests", () => {
    const registry = new PendingRegistry();
    const live = port();
    const first = registry.add("a", 1, live, "call");
    const second = registry.add("a", 2, live, "pick");
    registry.add("b", 1, live, "pick");
    expect(first).not.toBe(second);
    registry.purge("a");
    expect(registry.size).toBe(1);
    expect(registry.claim(first, () => live)).toBeNull();
    expect(registry.claim(second, () => live)).toBeNull();
  });

  it("ignores an unknown id", () => {
    const registry = new PendingRegistry();
    expect(registry.claim("a#99", () => port())).toBeNull();
  });
});

describe("parsePayload", () => {
  it("parses JSON payloads and degrades on errors", () => {
    expect(parsePayload({ seq: 1, payload: `{"a":1}`, error: null })).toEqual({ a: 1 });
    expect(parsePayload({ seq: 1, payload: "", error: "boom" })).toEqual({ error: "boom" });
    expect(parsePayload({ seq: 1, payload: "not json", error: null })).toBeNull();
  });
});
