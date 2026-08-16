
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

/// Parse an optional whole-token ceiling.
export function parseTokenBudget(text: string): number | null | undefined {
  const trimmed = text.trim().replace(/[_,\s]/g, '');
  if (trimmed === '') return null;
  if (!/^\d+$/.test(trimmed)) return undefined;
  const value = Number(trimmed);
  return Number.isSafeInteger(value) ? value : undefined;
}

/// Format an editable, round-trippable token count.
export function formatTokenBudget(count: number | null | undefined): string {
  if (count == null) return '';
  return String(count);
}

export function tokenBudgetHint(count: number | null | undefined): string {
  if (count == null) return 'Unlimited — this board spends the tokens its work takes.';
  if (count === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  return 'Runs are held once the day’s tokens reach this, and start again as soon as there is room. This is the ceiling that still bites on a subscription plan, where every run is billed at $0.';
}

/// Show the ceiling with the least remaining headroom, matching the server.
export function boardMeter(
  burn: { micros: number; tokens: number },
  ceilings: { micros: number | null | undefined; tokens: number | null | undefined },
): { used: number; ceiling: number | null; text: string } {
  const money = {
    used: burn.micros,
    ceiling: ceilings.micros ?? null,
    text:
      ceilings.micros == null
        ? formatUsd(burn.micros)
        : `${formatUsd(burn.micros)} / ${formatUsd(ceilings.micros)}`,
  };
  if (ceilings.tokens == null) return money;
  const tokens = {
    used: burn.tokens,
    ceiling: ceilings.tokens,
    text: `${formatTokens(burn.tokens)} / ${formatTokens(ceilings.tokens)}`,
  };
  if (ceilings.micros == null) return tokens;
  // Prefer an exhausted meter; otherwise compare proportional usage.
  const moneyOver = burn.micros >= ceilings.micros;
  const tokensOver = burn.tokens >= ceilings.tokens;
  if (moneyOver !== tokensOver) return moneyOver ? money : tokens;
  return burn.micros * ceilings.tokens > burn.tokens * ceilings.micros ? money : tokens;
}

export const BUDGET_REFUSAL =
  'Daily budget must be an amount in dollars, or empty for no ceiling.';

export const TOKEN_BUDGET_REFUSAL =
  'Daily token budget must be a whole number of tokens, or empty for no ceiling.';

export const HELD_RUN_NOTE = 'the project is over one of its daily ceilings';

export function heldOnBudget(count: number): string {
  return `${count} held on budget`;
}
