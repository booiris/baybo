import { useCallback, useRef, useState, type ReactNode } from 'react';
import { RiArrowDownSLine } from 'react-icons/ri';

import { useDismiss } from '../../components/useDismiss';

export type PickerOption = {
  value: string;
  /// The accessible name, and the rendering when `node` is absent.
  label: string;
  /// What the choice looks like when a word is not enough — a status pill, a
  /// face. The panel is where the operator sees what they are moving the
  /// card *to*, so a status list that spells its columns out in the board's
  /// own pills shows the pipeline rather than describing it.
  node?: ReactNode;
};

/// How tall the panel is allowed to get, and how much room it needs below
/// the trigger before it gives up and opens upward.
const PANEL_MAX_PX = 260;

/// A value you can press.
///
/// This was a transparent native `<select>` laid over the value it set. That
/// bought a labelled control for free and cost two things: an OS dropdown in
/// the middle of a hand-drawn rail, and a value that gave no sign it could be
/// pressed at all — the rail read as a table of facts, and three of its lines
/// were secretly controls. Now the value *is* the button, and the panel under
/// it is the board's own.
///
/// Deliberately not `role="listbox"`. The options are ordinary buttons, so Tab
/// and Enter reach them; claiming the listbox role without arrow-key roving
/// would advertise an interaction that is not implemented.
export function Picker({
  label,
  value,
  disabled,
  onPick,
  options,
  triggerClassName = '',
  className = '',
  panelClassName = '',
  title,
  children,
}: {
  label: string;
  value: string;
  disabled: boolean;
  onPick: (value: string) => void;
  options: PickerOption[];
  /// The trigger's own skin — a status pill, a bordered chip. The caret is
  /// added here, so callers style the value and not the affordance.
  triggerClassName?: string;
  /// The wrapper. A chip in a row wants none of this; a form field wants
  /// `w-full`, since a field that does not fill its row is not a field.
  className?: string;
  /// The panel. Anchored to the trigger's right edge by default, which is
  /// what a chip wants; a full-width field adds `left-0` so the panel spans
  /// the same width as the thing it belongs to.
  panelClassName?: string;
  title?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const [above, setAbove] = useState(false);
  const root = useRef<HTMLDivElement | null>(null);
  const trigger = useRef<HTMLButtonElement | null>(null);
  const current = options.find((option) => option.value === value);
  const close = useCallback(() => {
    setOpen(false);
  }, []);
  useDismiss({ open, root, trigger, onDismiss: close });

  return (
    <div ref={root} className={`relative inline-flex shrink-0 ${className}`}>
      <button
        ref={trigger}
        type="button"
        aria-haspopup="true"
        aria-expanded={open}
        aria-label={`${label}: ${current?.label ?? value}`}
        title={title}
        disabled={disabled}
        onClick={() => {
          if (!open) {
            const box = trigger.current?.getBoundingClientRect();
            // Both panes this sits in scroll, so a panel opened near the foot
            // of one is clipped rather than scrolled to.
            setAbove(box != null && window.innerHeight - box.bottom < PANEL_MAX_PX);
          }
          setOpen((showing) => !showing);
        }}
        className={`inline-flex items-center gap-0.5 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 ${triggerClassName}`}
      >
        {children}
        <RiArrowDownSLine aria-hidden className="shrink-0 opacity-60" />
      </button>
      {open ? (
        <div
          style={{ maxHeight: PANEL_MAX_PX }}
          className={`absolute right-0 z-30 min-w-[170px] overflow-y-auto overscroll-contain border-2 border-black rounded-md bg-surface shadow-brutal py-1 ${
            above ? 'bottom-[calc(100%+4px)]' : 'top-[calc(100%+4px)]'
          } ${panelClassName}`}
        >
          {options.map((option) => {
            const picked = option.value === value;
            return (
              <button
                key={option.value}
                type="button"
                onClick={() => {
                  setOpen(false);
                  if (!picked) onPick(option.value);
                }}
                className={`flex w-full items-center gap-2 px-2 py-1 text-left font-mono text-[0.66rem] cursor-pointer hover:bg-brand/35 ${
                  picked ? 'bg-brand/20' : ''
                }`}
              >
                <span
                  aria-hidden
                  className={`h-1.5 w-1.5 shrink-0 rounded-full ${picked ? 'bg-ink' : 'bg-transparent'}`}
                />
                {option.node ?? option.label}
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
