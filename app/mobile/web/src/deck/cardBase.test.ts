import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import sdkSource from "./sdkCard.js?raw";

// Read off disk rather than through `?raw`: vitest stubs every `.css` import
// to an empty string, so the idiom `shell.ts` uses to inline this file into a
// card's srcdoc yields nothing under test.
const baseCss = readFileSync("src/deck/cardBase.css", "utf8");

/// The `max` scroll surface lives in this injected base, not in the card, and
/// a card's own `<style>` comes later in the cascade — so only specificity
/// keeps these rules winning. They are pinned here because dropping `height`
/// or `overflow` from either one hands the scroll container back to whatever
/// the agent-written fragment declares, and a maximized card whose content
/// runs past the fold silently stops scrolling. Nothing else catches that:
/// the install gate never renders `card.html`.
const maxRule = (selector: string) => {
  const at = baseCss.indexOf(`html[data-deck-size="max"] ${selector} {`);
  expect(at, `no max rule for ${selector}`).toBeGreaterThan(-1);
  return baseCss.slice(at, baseCss.indexOf("}", at));
};

describe("card base stylesheet", () => {
  it("owns the max scroll surface on body", () => {
    const body = maxRule("body");
    expect(body).toMatch(/height:\s*auto/);
    expect(body).toMatch(/overflow:\s*visible/);
  });

  it("declares the box .card gets at max, so a card cannot strand content", () => {
    const card = maxRule(".card");
    expect(card).toMatch(/height:\s*auto/);
    expect(card).toMatch(/overflow:\s*visible/);
    expect(card).toMatch(/padding-top:\s*var\(--deck-header-clearance\)/);
    expect(card).toMatch(/padding-bottom:\s*var\(--deck-tab-bar-clearance\)/);
  });

  it("keys those rules off the attribute the SDK actually writes", () => {
    expect(sdkSource).toContain("dataset.deckSize");
    expect(baseCss).toContain('html[data-deck-size="max"]');
  });

  it("keeps a tile clipped, so only max scrolls", () => {
    expect(baseCss).toMatch(
      /body\s*\{[^}]*height:\s*100vh[^}]*overflow:\s*hidden/,
    );
  });
});
