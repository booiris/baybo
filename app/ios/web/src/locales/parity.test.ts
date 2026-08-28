import { describe, expect, it } from "vitest";

import { en } from "./en";
import { zh } from "./zh";

/// The two locales must carry the SAME keys. A key present in one and not the
/// other is invisible in review and invisible in the tests that assert on
/// English labels: i18next falls back to echoing the key, so the miss ships as
/// literal `chat.workedFor` on the other language's screen.
///
/// Written when `chat.stepsN` was deleted from both — the deletion that has to
/// stay in step is exactly the one nothing else would have caught.
function leaves(value: unknown, prefix = ""): string[] {
  if (typeof value !== "object" || value === null) return [prefix];
  return Object.entries(value).flatMap(([k, v]) => leaves(v, prefix === "" ? k : `${prefix}.${k}`));
}

describe("locale parity", () => {
  const enKeys = leaves(en).sort();
  const zhKeys = leaves(zh).sort();

  it("every Chinese key has an English one", () => {
    expect(zhKeys.filter((k) => !enKeys.includes(k))).toEqual([]);
  });

  it("and every English key has a Chinese one", () => {
    expect(enKeys.filter((k) => !zhKeys.includes(k))).toEqual([]);
  });

  it("no key is left empty — an empty string renders as nothing at all", () => {
    for (const bundle of [en, zh]) {
      const empty = leaves(bundle).filter((path) => {
        const value = path
          .split(".")
          .reduce<unknown>((acc, k) => (acc as Record<string, unknown> | undefined)?.[k], bundle);
        return typeof value === "string" && value.trim() === "";
      });
      expect(empty).toEqual([]);
    }
  });

  it("deck quick setup exercises ordinary-language skill selection", () => {
    expect(en.translation.deck.quickSetupPrompt).not.toMatch(/^\s*\//);
    expect(zh.translation.deck.quickSetupPrompt).not.toMatch(/^\s*\//);
    expect(en.translation.deck.empty).not.toContain("/deck");
    expect(zh.translation.deck.empty).not.toContain("/deck");
  });
});
