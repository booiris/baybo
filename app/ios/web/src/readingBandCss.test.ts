import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// Read off disk: vitest stubs `.css` imports to an empty string.
const css = readFileSync("src/styles.css", "utf8");

/// The band is for a TALL wide window. A phone in landscape is wider than
/// 720px but short, and keeps the full width it had before the band existed;
/// native asks the vertical size class for the same answer (`Theme.readingCap`).
describe("reading band stylesheet", () => {
  it("caps the band at 720px", () => {
    expect(css).toMatch(/--reading-width: 720px;/);
  });

  it("lifts the cap on a short window", () => {
    const at = css.indexOf("@media (max-height: 500px)");
    expect(at).toBeGreaterThan(-1);
    expect(css.slice(at)).toMatch(/^@media \(max-height: 500px\) \{\s*:root \{\s*--reading-width: 100%;/);
  });
});
