import { describe, expect, it } from 'vitest';

import { budgetHint, formatBudget, parseBudget } from './budgetModel';

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
