import { RiAlertFill, RiErrorWarningFill, RiInformationFill } from 'react-icons/ri';
import type { LogLevel } from '../types';

const styles: Record<LogLevel, string> = {
  error: 'text-err border-err',
  warn: 'text-warn border-warn',
  info: 'text-info border-info',
};

function iconFor(level: LogLevel) {
  if (level === 'error') return <RiErrorWarningFill />;
  if (level === 'warn') return <RiAlertFill />;
  return <RiInformationFill />;
}

export function Badge({ level }: { level: LogLevel }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-bold uppercase border-2 ${styles[level]}`}
    >
      {iconFor(level)}
      {level}
    </span>
  );
}
