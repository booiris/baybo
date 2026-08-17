
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

/// What a ceiling the day has already passed actually does.
///
/// Both hints below promise that runs "start again as soon as there is
/// room", and for a ceiling under what today has already spent that promise
/// is false: the release bails before it touches a row. Saving one is a
/// legitimate act — it is how a board is stopped — but it is *silent*. The
/// modal closes, the board repaints identically, nothing is written to any
/// timeline, and the held runs stay held. An operator who raised the ceiling
/// to fix exactly that reads the whole thing as the save not having worked.
const STAYS_STOPPED =
  ' Today has already spent this, so the board stays stopped and anything held stays held — until you go above the spend, or the day resets at 00:00 UTC.';

function alreadySpent(ceiling: number, spentToday: number | null | undefined): boolean {
  return spentToday != null && spentToday >= ceiling;
}

export function budgetHint(
  micros: number | null | undefined,
  spentToday?: number | null,
): string {
  if (micros == null) return 'No ceiling — this board spends what its work costs.';
  if (micros === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  const hint =
    'Runs are held once the day’s spend reaches this, and start again as soon as there is room.';
  return alreadySpent(micros, spentToday) ? hint + STAYS_STOPPED : hint;
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

export function tokenBudgetHint(
  count: number | null | undefined,
  spentToday?: number | null,
): string {
  if (count == null) return 'Unlimited — this board spends the tokens its work takes.';
  if (count === 0) {
    return 'Paused: work is recorded and held, and starts when you raise this.';
  }
  const hint =
    'Runs are held once the day’s tokens reach this, and start again as soon as there is room. This is the ceiling that still bites on a subscription plan, where every run is billed at $0.';
  return alreadySpent(count, spentToday) ? hint + STAYS_STOPPED : hint;
}

/// Every ceiling this board actually has, in the unit each one is set in.
///
/// [`boardMeter`] answers a different question — which single ceiling to
/// *speak in* — and that is right for a sentence, which can only be in one
/// unit. It is wrong for a readout: money and tokens are independent gates
/// and **either** one stops the board, so naming only the tighter of two
/// sends the operator to raise a ceiling that may not be the one biting, or
/// to raise one of two that are both biting and watch nothing happen.
///
/// A board with no ceiling at all still has a meter — what it has spent —
/// which is what the switcher's idle rows show.
export function boardMeters(
  burn: { micros: number; tokens: number },
  ceilings: { micros: number | null | undefined; tokens: number | null | undefined },
): { text: string; over: boolean }[] {
  const meters: { text: string; over: boolean }[] = [];
  if (ceilings.micros != null) {
    meters.push({
      text: `${formatUsd(burn.micros)} / ${formatUsd(ceilings.micros)}`,
      over: burn.micros >= ceilings.micros,
    });
  }
  if (ceilings.tokens != null) {
    meters.push({
      text: `${formatTokens(burn.tokens)} / ${formatTokens(ceilings.tokens)}`,
      over: burn.tokens >= ceilings.tokens,
    });
  }
  if (meters.length === 0) meters.push({ text: formatUsd(burn.micros), over: false });
  return meters;
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
