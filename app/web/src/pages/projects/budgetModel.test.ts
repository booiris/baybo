import { describe, expect, it } from 'vitest';

import {
  boardMeter,
  budgetHint,
  formatBudget,
  formatTokenBudget,
  formatTokens,
  formatUsd,
  parseBudget,
  parseTokenBudget,
  tokenBudgetHint,
} from './budgetModel';

describe('formatUsd', () => {
  it('shows ordinary spend to the cent', () => {
    expect(formatUsd(40_000)).toBe('$0.04');
    expect(formatUsd(310_000)).toBe('$0.31');
    expect(formatUsd(2_500_000)).toBe('$2.50');
  });

  it('never rounds real spend down to free', () => {
    // A run that cost a twentieth of a cent is cheap; "$0.00" is the one
    // reading that is wrong, because it says nothing was billed.
    expect(formatUsd(500)).not.toBe('$0.00');
    expect(Number(formatUsd(500).slice(1))).toBeGreaterThan(0);
    expect(Number(formatUsd(12).slice(1))).toBeGreaterThan(0);
  });

  it('separates nothing-spent from nothing-known', () => {
    expect(formatUsd(0)).toBe('$0.00');
    expect(formatUsd(null)).toBe('—');
    expect(formatUsd(undefined)).toBe('—');
  });
});

describe('formatTokens', () => {
  it('abbreviates thousands and keeps small counts exact', () => {
    expect(formatTokens(0)).toBe('0');
    expect(formatTokens(999)).toBe('999');
    expect(formatTokens(1_200)).toBe('1.2k');
    expect(formatTokens(42_000)).toBe('42k');
    expect(formatTokens(128_000)).toBe('128k');
  });
});

describe('parseBudget', () => {
  it('reads dollars as exact micro-USD', () => {
    expect(parseBudget('5')).toBe(5_000_000);
    expect(parseBudget('12.50')).toBe(12_500_000);
    expect(parseBudget('0.01')).toBe(10_000);
    expect(parseBudget('$7.25')).toBe(7_250_000);
    expect(parseBudget('  3  ')).toBe(3_000_000);
  });

  it('treats an empty box as no ceiling, not as zero', () => {
    expect(parseBudget('')).toBeNull();
    expect(parseBudget('   ')).toBeNull();
    expect(parseBudget('0')).toBe(0);
  });

  it('refuses text that is not a budget rather than guessing', () => {
    for (const bad of ['abc', '1.2.3', '-5', '1e6', '5 dollars', '.5']) {
      expect(parseBudget(bad), bad).toBeUndefined();
    }
  });

  it('drops digits past micro-USD instead of rounding them', () => {
    expect(parseBudget('1.0000009')).toBe(1_000_000);
    expect(parseBudget('1.1234567')).toBe(1_123_456);
  });
});

describe('formatBudget', () => {
  it('round-trips what parseBudget produced', () => {
    for (const text of ['5', '12.5', '0.01', '0']) {
      const micros = parseBudget(text);
      expect(typeof micros).toBe('number');
      expect(parseBudget(formatBudget(micros as number))).toBe(micros);
    }
  });

  it('shows a whole-dollar ceiling without six zeroes', () => {
    expect(formatBudget(5_000_000)).toBe('5');
    expect(formatBudget(12_500_000)).toBe('12.5');
    expect(formatBudget(0)).toBe('0');
  });

  it('renders no ceiling as an empty box', () => {
    expect(formatBudget(null)).toBe('');
    expect(formatBudget(undefined)).toBe('');
  });
});

describe('budgetHint', () => {
  it('separates no ceiling from a zero one', () => {
    expect(budgetHint(null)).toContain('No ceiling');
    expect(budgetHint(0)).toContain('Paused');
    expect(budgetHint(5_000_000)).toContain('held');
  });
});

describe('parseTokenBudget', () => {
  it('reads whole tokens, and empty as no ceiling', () => {
    expect(parseTokenBudget('250000')).toBe(250_000);
    expect(parseTokenBudget('0')).toBe(0);
    expect(parseTokenBudget('')).toBeNull();
    expect(parseTokenBudget('   ')).toBeNull();
  });

  it('accepts the separators an operator types into a six-figure number', () => {
    expect(parseTokenBudget('250,000')).toBe(250_000);
    expect(parseTokenBudget('250_000')).toBe(250_000);
    expect(parseTokenBudget('250 000')).toBe(250_000);
  });

  it('refuses anything that is not a count', () => {
    expect(parseTokenBudget('1.5')).toBeUndefined();
    expect(parseTokenBudget('250k')).toBeUndefined();
    expect(parseTokenBudget('-1')).toBeUndefined();
    expect(parseTokenBudget('lots')).toBeUndefined();
  });
});

describe('formatTokenBudget', () => {
  it('round-trips exact digits rather than the abbreviated form', () => {
    expect(formatTokenBudget(250_000)).toBe('250000');
    expect(parseTokenBudget(formatTokenBudget(250_000))).toBe(250_000);
  });

  it('renders no ceiling as an empty box', () => {
    expect(formatTokenBudget(null)).toBe('');
    expect(formatTokenBudget(undefined)).toBe('');
  });
});

describe('tokenBudgetHint', () => {
  it('separates no ceiling from a zero one', () => {
    expect(tokenBudgetHint(null)).toContain('Unlimited');
    expect(tokenBudgetHint(0)).toContain('Paused');
    expect(tokenBudgetHint(250_000)).toContain('held');
  });

  it('does not repeat the money hint\'s opening words', () => {
    expect(tokenBudgetHint(null)).not.toContain('No ceiling');
  });
});

describe('boardMeter', () => {
  it('shows whichever ceiling has least room left', () => {
    const tokensTight = boardMeter(
      { micros: 0, tokens: 96_000 },
      { micros: 5_000_000, tokens: 100_000 },
    );
    expect(tokensTight.text).toBe('96k / 100k');
    expect(tokensTight.used).toBe(96_000);
    expect(tokensTight.ceiling).toBe(100_000);
    expect(tokensTight.text).not.toContain('$');

    const moneyTight = boardMeter(
      { micros: 4_900_000, tokens: 50_000 },
      { micros: 5_000_000, tokens: 1_000_000_000 },
    );
    expect(moneyTight.text).toBe('$4.90 / $5.00');
    expect(moneyTight.ceiling).toBe(5_000_000);
  });

  it('agrees with the server about an exhausted ceiling, paused ones included', () => {
    const paused = boardMeter(
      { micros: 0, tokens: 50_000 },
      { micros: 0, tokens: 1_000_000 },
    );
    expect(paused.text).toBe('$0.00 / $0.00');
    expect(paused.ceiling).toBe(0);
  });

  it('falls back to money when only a money ceiling is set', () => {
    const meter = boardMeter({ micros: 1_840_000, tokens: 400 }, { micros: 5_000_000, tokens: null });
    expect(meter.text).toBe('$1.84 / $5.00');
    expect(meter.ceiling).toBe(5_000_000);
  });

  it('reports a burn with no ceiling to measure it against', () => {
    const meter = boardMeter({ micros: 1_840_000, tokens: 400 }, { micros: null, tokens: null });
    expect(meter.text).toBe('$1.84');
    expect(meter.ceiling).toBeNull();
  });

  it('shows a subscription board its tokens rather than $0.00 forever', () => {
    const meter = boardMeter({ micros: 0, tokens: 12_400 }, { micros: null, tokens: 50_000 });
    expect(meter.text).toBe('12k / 50k');
  });
});
