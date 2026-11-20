import { useEffect, useRef, useState } from 'react';
import { RiCheckLine, RiQuestionLine } from 'react-icons/ri';

import type { WireQuestion, WireQuestionAnswer, WireQuestionReply } from '../../api/chatWs';

export interface QuestionCardProps {
  askId: string;
  questions: WireQuestion[];
  activeTab: number;
  onTabChange: (index: number) => void;
  onSubmit: (reply: WireQuestionReply) => boolean;
  connected: boolean;
  queuedCount?: number;
}

interface TabDraft {
  selected: string[];
  other: string;
}

function isTabAnswered(draft: TabDraft | undefined): boolean {
  if (!draft) return false;
  return draft.selected.length > 0 || draft.other.trim().length > 0;
}

function isTextInputTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  if (target.tagName === 'TEXTAREA' || target.tagName === 'SELECT') return true;
  if (target instanceof HTMLInputElement) {
    return (
      target.type !== 'radio' &&
      target.type !== 'checkbox' &&
      target.type !== 'button' &&
      target.type !== 'submit'
    );
  }
  return false;
}

export function QuestionCard({
  askId,
  questions,
  activeTab,
  onTabChange,
  onSubmit,
  connected,
  queuedCount = 0,
}: QuestionCardProps) {
  const [drafts, setDrafts] = useState<TabDraft[]>(() =>
    questions.map(() => ({ selected: [], other: '' })),
  );
  const [submitting, setSubmitting] = useState(false);
  const [confirmingCancel, setConfirmingCancel] = useState(false);
  const submittingRef = useRef(false);
  const cardRef = useRef<HTMLDivElement>(null);

  const safeTab =
    questions.length > 0 ? Math.min(Math.max(0, activeTab), questions.length - 1) : 0;
  const current = questions[safeTab];
  const currentDraft = drafts[safeTab] ?? { selected: [], other: '' };
  const allAnswered =
    questions.length > 0 && questions.every((_, idx) => isTabAnswered(drafts[idx]));

  const toggleOption = (tabIdx: number, label: string, multiSelect: boolean) => {
    setDrafts((prev) =>
      prev.map((d, idx) => {
        if (idx !== tabIdx) return d;
        if (!multiSelect) {
          return { selected: [label], other: '' };
        }
        const exists = d.selected.includes(label);
        const nextSelected = exists
          ? d.selected.filter((item) => item !== label)
          : [...d.selected, label];
        return { ...d, selected: nextSelected };
      }),
    );
  };

  const setOtherText = (tabIdx: number, value: string, multiSelect: boolean) => {
    setDrafts((prev) =>
      prev.map((d, idx) => {
        if (idx !== tabIdx) return d;
        if (!multiSelect && value.trim().length > 0) {
          return { selected: [], other: value };
        }
        return { ...d, other: value };
      }),
    );
  };

  const sendReply = (reply: WireQuestionReply) => {
    if (submittingRef.current || !connected) return;
    submittingRef.current = true;
    const sent = onSubmit(reply);
    submittingRef.current = sent;
    setSubmitting(sent);
  };

  const submitAnswers = () => {
    if (!allAnswered) return;
    const answers: WireQuestionAnswer[] = questions.map((q, idx) => {
      const d = drafts[idx] ?? { selected: [], other: '' };
      const trimmedOther = d.other.trim();
      return {
        header: q.header,
        selected: d.selected,
        ...(trimmedOther.length > 0 ? { other: trimmedOther } : {}),
      };
    });
    sendReply({ kind: 'answered', answers });
  };

  const stateRef = useRef({
    questions,
    safeTab,
    onTabChange,
  });
  stateRef.current = {
    questions,
    safeTab,
    onTabChange,
  };

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
      if (isTextInputTarget(e.target)) return;
      if (e.target instanceof HTMLElement && e.target !== document.body && cardRef.current?.contains(e.target) !== true) return;
      const {
        questions: qs,
        safeTab: tab,
        onTabChange: setTab,
      } = stateRef.current;
      if (qs.length === 0) return;

      if ((e.key === 'ArrowRight' || e.key === 'ArrowLeft') && qs.length > 1) {
        e.preventDefault();
        const delta = e.key === 'ArrowRight' ? 1 : -1;
        const next = (tab + delta + qs.length) % qs.length;
        setTab(next);
        cardRef.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]')[next]?.focus();
        return;
      }
      if (e.key >= '1' && e.key <= '4') {
        const optIdx = Number(e.key) - 1;
        if (optIdx < qs[tab].options.length && !submittingRef.current) {
          e.preventDefault();
          if (!e.repeat) cardRef.current?.querySelectorAll<HTMLInputElement>('input')[optIdx]?.click();
        }
        return;
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);

  if (questions.length === 0) return null;

  const submitDisabled = submitting || !connected || !allAnswered;

  return (
    <div ref={cardRef} className="border-2 border-black bg-white rounded-md shadow-brutal-sm p-3 flex flex-col gap-2.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="font-bold uppercase tracking-wider text-sm flex items-center gap-1.5">
          <RiQuestionLine className="text-base shrink-0" />
          Question
        </span>
        {queuedCount > 0 ? (
          <span className="shrink-0 px-1.5 py-0.5 rounded border border-black bg-canvas font-mono text-[11px] font-bold">
            +{queuedCount} waiting
          </span>
        ) : null}
      </div>

      <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="Questions">
        {questions.map((q, idx) => {
          const active = idx === safeTab;
          const answered = isTabAnswered(drafts[idx]);
          return (
            <button
              key={`${q.header}-${idx}`}
              type="button"
              role="tab"
              aria-selected={active}
              onFocus={() => onTabChange(idx)}
              onClick={() => onTabChange(idx)}
              className={`px-2 py-0.5 border-2 rounded font-mono text-xs flex items-center gap-1 cursor-pointer transition-colors ${
                active
                  ? 'border-black bg-brand text-ink font-bold'
                  : 'border-black/30 bg-canvas text-ink-soft hover:border-black hover:text-ink'
              }`}
            >
              {answered ? <RiCheckLine className="text-xs shrink-0" aria-label="answered" /> : null}
              <span>{q.header}</span>
            </button>
          );
        })}
      </div>

      <div className="text-sm font-medium text-ink break-words [overflow-wrap:anywhere]">
        {current.question}
      </div>

      <div className="flex flex-col gap-1.5" role={current.multi_select ? 'group' : 'radiogroup'}>
        {current.options.map((opt, optIdx) => {
          const checked = currentDraft.selected.includes(opt.label);
          return (
            <label
              key={`${opt.label}-${optIdx}`}
              className={`flex items-start gap-2 px-2.5 py-2 border-2 rounded-md cursor-pointer transition-colors ${
                checked
                  ? 'border-black bg-brand/20'
                  : 'border-black/25 bg-canvas/50 hover:border-black'
              }`}
            >
              <input
                type={current.multi_select ? 'checkbox' : 'radio'}
                name={`question-${askId}-${safeTab}`}
                checked={checked}
                disabled={submitting}
                onChange={() => toggleOption(safeTab, opt.label, current.multi_select)}
                onClick={() => {
                  if (!current.multi_select && safeTab + 1 < questions.length) {
                    cardRef.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]')[safeTab + 1]?.focus();
                  }
                }}
                className="mt-0.5 shrink-0 accent-black cursor-pointer"
              />
              <span className="flex flex-col min-w-0 flex-1">
                <span className="text-sm font-semibold text-ink break-words [overflow-wrap:anywhere]">
                  <span className="font-mono text-[11px] text-ink-soft mr-1.5">
                    {optIdx + 1}.
                  </span>
                  {opt.label}
                </span>
                {opt.description ? (
                  <span className="text-xs text-ink-soft break-words [overflow-wrap:anywhere]">
                    {opt.description}
                  </span>
                ) : null}
              </span>
            </label>
          );
        })}
      </div>

      <textarea
        rows={2}
        value={currentDraft.other}
        disabled={submitting}
        onChange={(e) => setOtherText(safeTab, e.target.value, current.multi_select)}
        placeholder="Other…"
        aria-label={`Other answer for ${current.header}`}
        className="w-full resize-y px-2.5 py-1.5 border-2 border-black/40 focus:border-black rounded-md bg-white text-sm font-sans focus:outline-none placeholder:text-ink-soft/70"
      />

      {!connected ? (
        <div className="text-[0.7rem] font-mono uppercase tracking-wider text-warn">
          Waiting for connection — buttons disabled until the WS is back.
        </div>
      ) : null}

      <div className="flex items-center justify-between gap-2 flex-wrap pt-0.5">
        <div className="flex gap-2 flex-wrap">
          <button
            type="button"
            onClick={submitAnswers}
            disabled={submitDisabled}
            className="px-3 py-1.5 bg-brand text-ink border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:bg-brand-hover active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer flex items-center gap-1 disabled:opacity-50 disabled:cursor-not-allowed disabled:active:translate-x-0 disabled:active:translate-y-0 disabled:active:shadow-brutal-sm"
          >
            <RiCheckLine /> Submit
          </button>
          <button
            type="button"
            onClick={(event) => {
              if (confirmingCancel) {
                setConfirmingCancel(false);
                sendReply({ kind: 'cancelled' });
              } else {
                event.currentTarget.focus();
                setConfirmingCancel(true);
              }
            }}
            onBlur={() => setConfirmingCancel(false)}
            onKeyDown={(event) => {
              if (event.key === 'Escape') setConfirmingCancel(false);
            }}
            disabled={submitting || !connected}
            className="px-3 py-1.5 bg-white text-ink border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:bg-gray-100 active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:active:translate-x-0 disabled:active:translate-y-0 disabled:active:shadow-brutal-sm"
          >
            {confirmingCancel ? 'Confirm cancel?' : 'Cancel ask'}
          </button>
        </div>
        {questions.length > 1 ? <span className="text-[11px] font-mono text-ink-soft">←/→ tabs</span> : null}
      </div>
    </div>
  );
}
