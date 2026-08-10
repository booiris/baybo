
const PER_USD = 1_000_000;
const FRACTION_DIGITS = 6;

export function parseBudget(text: string): number | null | undefined {
  const trimmed = text.trim().replace(/^\$/, '');
  if (trimmed === '') return null;
  if (!/^\d+(\.\d+)?$/.test(trimmed)) return undefined;
  const [whole, fraction = ''] = trimmed.split('.');
  const micros = fraction.padEnd(FRACTION_DIGITS, '0').slice(0, FRACTION_DIGITS);
  const value = Number(whole) * PER_USD + Number(micros);
  return Number.isSafeInteger(value) ? value : undefined;
}

export function formatBudget(micros: number | null | undefined): string {
  if (micros == null) return '';
  const whole = Math.trunc(micros / PER_USD);
  const fraction = Math.abs(micros % PER_USD);
  if (fraction === 0) return String(whole);
  return `${whole}.${String(fraction).padStart(FRACTION_DIGITS, '0').replace(/0+$/, '')}`;
}

/// Money as spent, not as typed: always a `$`, and never rounded down to
/// `$0.00`. A run that cost a twentieth of a cent is cheap, but showing it
/// as free is the one reading that is wrong — so anything under a cent
/// keeps enough digits to stay non-zero.
export function formatUsd(micros: number | null | undefined): string {
  if (micros == null) return '—';
  const usd = micros / PER_USD;
  if (usd === 0) return '$0.00';
  if (Math.abs(usd) < 0.01) {
    const digits = Math.min(FRACTION_DIGITS, Math.ceil(-Math.log10(Math.abs(usd))) + 1);
    return `$${usd.toFixed(digits)}`;
  }
  return `$${usd.toFixed(2)}`;
}

/// Token counts as the rail shows them: thousands abbreviated, because the
/// exact digit is never the question and a six-figure number would wrap the
/// row it shares with its label.
export function formatTokens(count: number): string {
  if (count < 1000) return String(count);
  const thousands = count / 1000;
  return `${thousands < 10 ? thousands.toFixed(1) : Math.round(thousands)}k`;
}

export function budgetHint(micros: number | null | undefined): string {
  if (micros == null) return 'No ceiling — this board spends what its work costs.';
  if (micros === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  return 'Runs are held once the day’s spend reaches this, and start again as soon as there is room.';
}
