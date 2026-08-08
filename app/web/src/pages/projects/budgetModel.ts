
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

export function budgetHint(micros: number | null | undefined): string {
  if (micros == null) return 'No ceiling — this board spends what its work costs.';
  if (micros === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  return 'Runs are held once the day’s spend reaches this, and start again as soon as there is room.';
}
