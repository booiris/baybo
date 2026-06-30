import type { components } from '../api/schema';

type GoalItem = components['schemas']['GoalItem'];

const STATUS_STYLE: Record<string, string> = {
  active: 'bg-brand text-ink',
  complete: 'bg-ok text-white',
  blocked: 'bg-err text-white',
  paused: 'bg-selected text-ink',
  budget_limited: 'bg-warn text-white',
  spend_capped: 'bg-warn text-white',
};

/** Tailwind classes for a goal status pill; falls back for an unknown status. */
export function statusPillClass(status: string): string {
  return STATUS_STYLE[status] ?? 'bg-white text-ink';
}

/** `budget_limited` → `budget limited`. */
export function statusLabel(status: string): string {
  return status.replace('_', ' ');
}

/** `Hh Mm` / `Mm Ss` / `Ss` duration for goal usage display. */
export function fmtDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

/** `used` or `used / budget` (locale-grouped); callers add any unit suffix. */
export function fmtTokens(goal: GoalItem): string {
  const used = goal.tokens_used.toLocaleString();
  if (goal.token_budget == null) return used;
  return `${used} / ${goal.token_budget.toLocaleString()}`;
}
