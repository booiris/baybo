export function fmtPct(rate: number): string {
  return `${(rate * 100).toFixed(1)}%`;
}

export function fmtMs(ms: number | null | undefined): string {
  if (ms == null) return '—';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1000);
  return `${m}m${s.toString().padStart(2, '0')}s`;
}

export function fmtCost(micro: number | null | undefined): string {
  if (micro == null) return '—';
  return `$${(micro / 1_000_000).toFixed(4)}`;
}

export function fmtTokens(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}

export function fmtNum(n: number | null | undefined): string {
  return n == null ? '—' : n.toLocaleString();
}

/** Human byte size (`166 MB`, `6.6 MB`, `812 KB`, `40 B`), or `—`. */
export function fmtBytes(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n >= 1_048_576) return `${(n / 1_048_576).toFixed(1)} MB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KB`;
  return `${n} B`;
}

/** Cached-input share as a percent string (`88%`), or `—`. */
export function cacheRatePct(
  cached: number | null | undefined,
  input: number | null | undefined,
): string {
  if (cached == null || input == null || input <= 0) return '—';
  return `${Math.round((cached / input) * 100)}%`;
}

/**
 * `in / out` token pair, with `· N cached (RATE%)` appended when a cached
 * count is present (RATE = cached / input).
 */
export function fmtTokensCell(
  input: number | null | undefined,
  output: number | null | undefined,
  cached?: number | null,
): string {
  const base = `${fmtTokens(input)} / ${fmtTokens(output)}`;
  if (cached == null || cached <= 0) return base;
  const rate = input && input > 0 ? ` (${Math.round((cached / input) * 100)}%)` : '';
  return `${base} · ${fmtTokens(cached)} cached${rate}`;
}

/** Compact local date-time, or `—`. */
export function fmtTime(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  });
}

/** Short relative age (`3h ago`, `2d ago`) for the home cards. */
export function relTime(iso: string | null | undefined): string {
  if (!iso) return 'never';
  const d = new Date(iso).getTime();
  if (Number.isNaN(d)) return iso;
  const diff = Date.now() - d;
  const min = Math.floor(diff / 60_000);
  if (min < 1) return 'just now';
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return `${day}d ago`;
}
