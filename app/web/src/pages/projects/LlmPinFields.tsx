import { Picker } from './Picker';
import {
  effortOptions,
  llmOptions,
  modelOptions,
  type LlmPinValue,
  type ModelPool,
} from './teamModel';

/// What an agent runs on, as three fields: the `baybo.json` entry, the model
/// within it, and how hard it thinks.
///
/// One component, because both surfaces that set a pin — the profile panel
/// and the hire form — must offer the same three rows with the same rules,
/// and the rules are the sort that go wrong quietly when they are written
/// twice: a model is only pickable within an entry, changing the entry
/// invalidates the model, and a provider baybo sends no effort to must be
/// offered no ladder rather than a ladder that does nothing.
///
/// The pin is edited **whole**. Every change reports the complete triple, so
/// the caller does one full-replace write — a per-field save could leave the
/// row naming a model that belongs to an entry it no longer names, which is
/// the state the server-side `LlmPin` exists to prevent.
export function LlmPinFields({
  value,
  pool,
  disabled,
  fieldLabelClass,
  pickerProps,
  onChange,
}: {
  value: LlmPinValue;
  pool: ModelPool;
  disabled: boolean;
  /// The caller's own label skin — this group sits in a 320px side panel and
  /// in a modal form, which spell their labels differently.
  fieldLabelClass: string;
  /// The caller's `Picker` chrome (`className` / `triggerClassName` /
  /// `panelClassName`), so the three fields wear the surrounding form's box
  /// rather than one of their own.
  pickerProps: {
    className?: string;
    triggerClassName?: string;
    panelClassName?: string;
  };
  onChange: (next: LlmPinValue) => void;
}) {
  const entries = llmOptions(pool, value.llm);
  const models = modelOptions(pool, value.llm, value.model);
  const efforts = effortOptions(pool, value.llm, value.effort);
  const loading = pool === null;
  const label = (rows: { value: string; label: string }[], picked: string) =>
    rows.find((row) => row.value === picked)?.label ?? picked;

  return (
    <>
      <label className="flex flex-col gap-1">
        <span className={fieldLabelClass}>llm</span>
        {/* Pool-only: a pin outside it is a teammate that fails every time
            it is woken, so the picker never offers one — it only keeps a
            stale one visible enough to clear. */}
        <Picker
          {...pickerProps}
          label="llm"
          value={value.llm}
          disabled={disabled || loading}
          options={entries}
          onPick={(llm) => {
            // The model goes with the entry it belonged to. Carrying it
            // across would pin a model the new entry cannot serve, which the
            // gateway refuses — so the picker refuses it first.
            onChange({ llm, model: '', effort: value.effort });
          }}
        >
          {loading ? '…' : label(entries, value.llm)}
        </Picker>
      </label>

      <label className="flex flex-col gap-1">
        <span className={fieldLabelClass}>model</span>
        <Picker
          {...pickerProps}
          label="model"
          value={value.model}
          disabled={disabled || loading || models.length === 0}
          options={models}
          onPick={(model) => {
            onChange({ ...value, model });
          }}
        >
          {loading ? '…' : label(models, value.model)}
        </Picker>
      </label>

      {/* No rungs means this provider is sent no effort at all, so there is
          nothing to offer — a disabled row would advertise a knob that does
          not exist. */}
      {efforts.length === 0 ? null : (
        <label className="flex flex-col gap-1">
          <span className={fieldLabelClass}>thinking</span>
          <Picker
            {...pickerProps}
            label="thinking"
            value={value.effort}
            disabled={disabled}
            options={efforts}
            onPick={(effort) => {
              onChange({ ...value, effort });
            }}
          >
            {label(efforts, value.effort)}
          </Picker>
        </label>
      )}
    </>
  );
}
