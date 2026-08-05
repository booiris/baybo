/**
 * The daily budget as a person types it, and as the API stores it.
 *
 * The wire value is integer micro-USD, because money never rides a float
 * here — a budget compared with rounding error is a budget that disagrees
 * with the ledger it is measured against. So the conversion is done on the
 * decimal *string*, digit by digit, rather than by multiplying a parsed
 * float by a million.
 */

/** Micro-USD per dollar. Mirrors `MicroUsd::PER_USD`. */
const PER_USD = 1_000_000;
const FRACTION_DIGITS = 6;

/**
 * Parse what the operator typed.
 *
 * `null` means "no ceiling" — an empty box, which is the default and not an
 * error. `undefined` means the text is not a budget, which the form shows
 * rather than silently sending something else.
 */
export function parseBudget(text: string): number | null | undefined {
  const trimmed = text.trim().replace(/^\$/, '');
  if (trimmed === '') return null;
  if (!/^\d+(\.\d+)?$/.test(trimmed)) return undefined;
  const [whole, fraction = ''] = trimmed.split('.');
  // Pad or truncate to exactly six digits, so "1.5" is 1_500_000 and a
  // seventh decimal is dropped rather than rounding a value the operator
  // cannot see the effect of.
  const micros = fraction.padEnd(FRACTION_DIGITS, '0').slice(0, FRACTION_DIGITS);
  const value = Number(whole) * PER_USD + Number(micros);
  return Number.isSafeInteger(value) ? value : undefined;
}

/** Render a stored ceiling back into the box the operator types in. */
export function formatBudget(micros: number | null | undefined): string {
  if (micros == null) return '';
  const whole = Math.trunc(micros / PER_USD);
  const fraction = Math.abs(micros % PER_USD);
  if (fraction === 0) return String(whole);
  // Trailing zeroes off, so a $5 ceiling reads "5" and not "5.000000".
  return `${whole}.${String(fraction).padStart(FRACTION_DIGITS, '0').replace(/0+$/, '')}`;
}

/** What the ceiling means, said in one line under the field. */
export function budgetHint(micros: number | null | undefined): string {
  if (micros == null) return 'No ceiling — this board spends what its work costs.';
  if (micros === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  return 'Runs are held once the day’s spend reaches this, and start again when it rises or the day rolls over.';
}
