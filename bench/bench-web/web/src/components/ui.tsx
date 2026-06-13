import { RiCheckLine, RiCloseLine, RiLoader4Line } from 'react-icons/ri';
import { fmtPct } from '../lib/format';

/** Neo-brutalist box. */
export function Card({
  children,
  className = '',
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={`border-[3px] border-black rounded-md shadow-brutal bg-white ${className}`}>
      {children}
    </div>
  );
}

export function StatusPill({ passed }: { passed: boolean }) {
  return (
    <span
      className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-md border-2 border-black text-[0.7rem] font-bold uppercase tracking-wider ${
        passed ? 'bg-ok/15 text-ok' : 'bg-err/15 text-err'
      }`}
    >
      {passed ? <RiCheckLine /> : <RiCloseLine />}
      {passed ? 'pass' : 'fail'}
    </span>
  );
}

/** Horizontal pass-rate bar with an inline `n/total · pct` label. */
export function PassRateBar({
  rate,
  passed,
  total,
}: {
  rate: number;
  passed: number;
  total: number;
}) {
  const pct = Math.max(0, Math.min(100, rate * 100));
  const color = rate >= 0.999 ? 'bg-ok' : rate <= 0.001 ? 'bg-err' : 'bg-brand';
  return (
    <div className="flex items-center gap-2">
      <div className="relative h-4 w-32 border-2 border-black rounded bg-canvas overflow-hidden">
        <div className={`absolute inset-y-0 left-0 ${color}`} style={{ width: `${pct}%` }} />
      </div>
      <span className="font-mono text-[0.8rem] text-ink-soft whitespace-nowrap">
        {passed}/{total} · {fmtPct(rate)}
      </span>
    </div>
  );
}

/** Small labelled metric. */
export function StatChip({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="border-2 border-black rounded-md px-3 py-2 bg-white shadow-brutal-xs">
      <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold">{label}</div>
      <div className="font-mono text-[0.95rem] font-bold">{value}</div>
    </div>
  );
}

export function Spinner({ label = 'loading…' }: { label?: string }) {
  return (
    <div className="flex items-center gap-2 text-ink-soft text-[0.85rem] p-4">
      <RiLoader4Line className="animate-spin" />
      {label}
    </div>
  );
}

export function ErrorBox({ message }: { message: string }) {
  return (
    <div className="border-[3px] border-black rounded-md bg-err/10 text-err p-4 font-mono text-[0.85rem] shadow-brutal-sm break-words">
      {message}
    </div>
  );
}

export function Empty({ message }: { message: string }) {
  return <div className="text-ink-soft text-[0.85rem] p-4 italic">{message}</div>;
}
