import type { TargetsMsg } from '../../api/browserViewTypes';

// Read-only: the view follows whichever tab the agent used last, so the list
// says which one that is rather than offering a picker.
export function TargetList({ targets }: { targets: TargetsMsg | null }) {
  const list = targets?.targets ?? [];
  return (
    <section aria-label="Open tabs" className="flex flex-col min-h-0">
      <h3 className="text-xs font-bold uppercase tracking-wider text-ink-soft mb-2">
        Tabs{list.length > 0 ? ` · ${list.length}` : ''}
      </h3>
      {list.length === 0 ? (
        <p className="font-mono text-xs text-ink-soft">No tabs open.</p>
      ) : (
        <ul className="flex flex-col gap-1.5 overflow-y-auto min-h-0">
          {list.map((t) => {
            const followed = t.target_id === targets?.followed;
            return (
              <li
                key={t.target_id}
                aria-current={followed ? 'true' : undefined}
                className={`border-2 rounded-brutal px-2 py-1.5 font-mono text-xs ${
                  followed ? 'bg-brand/60 border-black shadow-brutal-xs' : 'border-black/20 bg-surface'
                }`}
              >
                <div className="flex items-center gap-1.5">
                  {followed ? (
                    <span className="shrink-0 text-[0.65rem] font-bold uppercase tracking-wider">Viewing</span>
                  ) : null}
                  <span className="font-bold truncate">{t.title || 'Untitled'}</span>
                </div>
                <div className="text-ink-soft truncate" title={t.url}>{t.url}</div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
