const MAX_BOT_ID_LEN = 64;

/**
 * Normalize an iLink bot id (e.g. `b0f5860fdecb@im.bot`) into a string
 * that passes Baybo's CLI validation (alphanumerics, `-`, `_`; ≤64
 * chars). `@` and `.` become `-`; anything outside the allowed charset
 * is dropped. Empty output throws — prefer failing loudly over silently
 * producing an empty key.
 */
export function normalizeBotId(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed) {
    throw new Error("normalizeBotId: empty input");
  }
  const replaced = trimmed.replace(/[@.]/g, "-");
  const filtered = replaced
    .split("")
    .filter((c) => /[A-Za-z0-9_-]/.test(c))
    .join("");
  if (!filtered) {
    throw new Error(`normalizeBotId: no valid characters in ${JSON.stringify(raw)}`);
  }
  return filtered.slice(0, MAX_BOT_ID_LEN);
}
