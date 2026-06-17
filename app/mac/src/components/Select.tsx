import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

// Custom dropdown matching the warm neo-brutalist style. Project convention
// (app/mac/CLAUDE.md): never use the native <select> — its popup can't be
// themed and clashes with the design. Use this component instead.
//
// The options panel renders in a portal with fixed positioning so it is never
// clipped by an `overflow` ancestor (e.g. the setup wizard's scroll area), and
// flips above the trigger when there isn't room below.

export interface SelectOption {
  value: string;
  label: string;
  hint?: string;
}

// `md` matches the text-input height (px-4 py-3) so selects line up with
// inputs; `sm` is for tight spots like the chat header.
const SIZES = {
  md: { trigger: 'px-4 py-3 text-sm', option: 'px-4 py-2.5 text-sm' },
  sm: { trigger: 'px-3 py-1.5 text-xs', option: 'px-3 py-1.5 text-xs' },
} as const;

const PANEL_MAX = 240;
const GAP = 4;

interface PanelPos {
  left: number;
  width: number;
  maxHeight: number;
  top?: number;
  bottom?: number;
}

export function Select({
  value,
  options,
  onChange,
  placeholder,
  className,
  size = 'md',
}: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  placeholder?: string;
  className?: string;
  size?: keyof typeof SIZES;
}) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<PanelPos | null>(null);
  const triggerRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const sz = SIZES[size];

  const measure = useCallback(() => {
    const el = triggerRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const spaceBelow = window.innerHeight - r.bottom;
    const spaceAbove = r.top;
    const flipUp = spaceBelow < Math.min(PANEL_MAX, 160) && spaceAbove > spaceBelow;
    setPos({
      left: r.left,
      width: r.width,
      maxHeight: Math.min(PANEL_MAX, (flipUp ? spaceAbove : spaceBelow) - GAP * 2),
      ...(flipUp
        ? { bottom: window.innerHeight - r.top + GAP }
        : { top: r.bottom + GAP }),
    });
  }, []);

  useLayoutEffect(() => {
    if (open) measure();
  }, [open, measure]);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node;
      if (triggerRef.current?.contains(t) || panelRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    // Reposition while open so the panel stays attached to the trigger if an
    // ancestor scrolls (capture catches nested scroll containers).
    window.addEventListener('scroll', measure, true);
    window.addEventListener('resize', measure);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('scroll', measure, true);
      window.removeEventListener('resize', measure);
    };
  }, [open, measure]);

  const selected = options.find((o) => o.value === value);

  return (
    <div ref={triggerRef} className={`relative ${className ?? ''}`}>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={`flex w-full items-center justify-between gap-2 rounded-brutal border-[3px] border-border bg-canvas text-left font-mono focus:shadow-brutal-sm focus:outline-none ${sz.trigger}`}
      >
        <span className="truncate">{selected?.label ?? placeholder ?? 'Select…'}</span>
        <span className={`shrink-0 text-xs transition-transform ${open ? 'rotate-180' : ''}`}>▾</span>
      </button>

      {open &&
        pos &&
        createPortal(
          <div
            ref={panelRef}
            style={{
              position: 'fixed',
              left: pos.left,
              top: pos.top,
              bottom: pos.bottom,
              width: pos.width,
              maxHeight: pos.maxHeight,
            }}
            className="no-scrollbar z-50 overflow-y-auto rounded-brutal border-[3px] border-border bg-surface shadow-brutal"
          >
            {options.map((o) => (
              <button
                key={o.value}
                type="button"
                onClick={() => {
                  onChange(o.value);
                  setOpen(false);
                }}
                className={`flex w-full items-center justify-between gap-2 text-left font-mono hover:bg-canvas ${sz.option} ${
                  o.value === value ? 'bg-selected font-bold' : ''
                }`}
              >
                <span className="truncate">{o.label}</span>
                {o.hint && <span className="shrink-0 text-xs text-ink-soft">{o.hint}</span>}
              </button>
            ))}
          </div>,
          document.body,
        )}
    </div>
  );
}
