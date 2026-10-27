import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// Read off disk: vitest stubs `.css` imports to an empty string.
const css = readFileSync("src/deck/deck.css", "utf8");

const rule = (selector: string) => {
  const at = css.indexOf(`${selector} {`);
  expect(at, `no rule for ${selector}`).toBeGreaterThan(-1);
  return css.slice(at, css.indexOf("}", at));
};

/// The deck webview is full-bleed (`DeckScreen` ignores the safe area), and on
/// iPhone Duo — landscape camera inset, Split View on the inner display — the
/// left and right insets differ. Each side must clear its OWN inset.
describe("deck shell stylesheet", () => {
  it("pads each grid side by its own safe-area inset", () => {
    const grid = rule(".deck-grid");
    expect(grid).toMatch(/max\(var\(--deck-gutter\), env\(safe-area-inset-left\)\)/);
    expect(grid).toMatch(/max\(var\(--deck-gutter\), env\(safe-area-inset-right\)\)/);
  });

  it("caps the 2-column grid on a regular-width window", () => {
    const grid = rule(".deck-grid");
    expect(grid).toMatch(/grid-template-columns:\s*repeat\(2, 1fr\)/);
    expect(grid).toMatch(/max-width:\s*var\(--deck-grid-max-width\)/);
    expect(grid).toMatch(/margin:\s*0 auto/);
  });

  it("lifts the cap on a short window, as a phone in landscape", () => {
    const short = css.slice(css.indexOf("@media (max-height: 500px)"));
    expect(short).toMatch(/^@media \(max-height: 500px\) \{\s*:root \{\s*--deck-grid-max-width: none;/);
  });

  it("insets a maximized card's content from both side insets", () => {
    const full = rule(".deck-card.maximized.deck-max-full");
    expect(full).toMatch(/padding-left:\s*env\(safe-area-inset-left\)/);
    expect(full).toMatch(/padding-right:\s*env\(safe-area-inset-right\)/);
    // Padding must not grow the full-viewport tile past the screen edge.
    expect(rule(".deck-card.maximized")).toMatch(/box-sizing:\s*border-box/);
  });
});
