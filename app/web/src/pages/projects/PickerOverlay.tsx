import type { ReactNode } from 'react';

/// A control that *looks* like the value it sets.
///
/// The mockup's step row is a status pill and a face, and its property rail
/// is a `label ── value` table — neither is a stack of form widgets. So the
/// picker is a real `<select>` laid transparently over the thing it edits:
/// the eye gets the mockup's row, the keyboard and the screen reader still
/// get a labelled native control.
export function PickerOverlay({
  label,
  value,
  disabled,
  onPick,
  options,
  className = 'inline-flex shrink-0',
  children,
}: {
  label: string;
  value: string;
  disabled: boolean;
  onPick: (value: string) => void;
  options: { value: string; label: string }[];
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`relative ${className}`}>
      {children}
      <select
        aria-label={label}
        value={value}
        disabled={disabled}
        onChange={(event) => {
          onPick(event.target.value);
        }}
        className="absolute inset-0 h-full w-full cursor-pointer opacity-0 disabled:cursor-not-allowed"
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </div>
  );
}
