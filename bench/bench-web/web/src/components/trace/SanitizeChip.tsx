import { RiLockLine } from 'react-icons/ri';
import type { SecretKind } from '../../types/trace';

const KIND_LABEL: Record<SecretKind, string> = {
  api_key: 'api key',
  bearer_token: 'bearer',
  aws_access_key: 'aws access',
  aws_secret_key: 'aws secret',
  private_key: 'private key',
  password: 'password',
  high_entropy: 'high entropy',
  other: 'secret',
};

export function SanitizeChip({ kind }: { kind: SecretKind }) {
  return (
    <span
      className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded-md border-2 border-black bg-warn/15 text-warn text-[0.7rem] font-bold uppercase tracking-wider align-baseline"
      title={`Sanitized ${kind}`}
    >
      <RiLockLine className="text-[0.85rem]" />
      {KIND_LABEL[kind] ?? kind}
    </span>
  );
}

const PLACEHOLDER_RE = /\[\{REDACTED_SECRET_[a-zA-Z0-9_-]+\}\]/g;

/**
 * Render a string with any `[{REDACTED_SECRET_*}]` placeholders
 * replaced inline by a SanitizeChip. The chip kind is best-effort:
 * the placeholder string itself doesn't carry the kind, so the
 * caller may pass a `kindHint` derived from the span's
 * `SanitizeHit` event (typically the first kind in the list).
 */
export function renderWithSanitizeChips(
  text: string,
  kindHint: SecretKind = 'other',
): React.ReactNode {
  if (!text.includes('[{REDACTED_SECRET_')) return text;
  const parts: React.ReactNode[] = [];
  let lastIndex = 0;
  let m: RegExpExecArray | null;
  PLACEHOLDER_RE.lastIndex = 0;
  while ((m = PLACEHOLDER_RE.exec(text)) !== null) {
    if (m.index > lastIndex) parts.push(text.slice(lastIndex, m.index));
    parts.push(<SanitizeChip key={`sc-${m.index}`} kind={kindHint} />);
    lastIndex = m.index + m[0].length;
  }
  if (lastIndex < text.length) parts.push(text.slice(lastIndex));
  return parts;
}
