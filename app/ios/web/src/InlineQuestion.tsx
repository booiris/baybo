import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { postToNative } from "./bridge";
import type { InlineQuestionPrompt } from "./types";

const SELECTION_HOLD_MS = 220;

export function InlineQuestion({ prompt, sessionId }: { prompt: InlineQuestionPrompt; sessionId: string }) {
  const { t } = useTranslation();
  const { active, tabs, question, answer, submitting, connected } = prompt;
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [pendingChoice, setPendingChoice] = useState<{ tab: number; index: number; label: string } | null>(null);
  const tabStrip = useRef<HTMLDivElement>(null);
  const id = useId();
  const last = active === tabs.length - 1;
  const askId = prompt.ask_id;
  const selectedChoice = pendingChoice?.tab === active && prompt.auto_advance && !submitting
    && answer.selected.includes(pendingChoice.label) ? pendingChoice : null;

  useEffect(() => {
    if (!selectedChoice) return;
    let timer: number | undefined;
    const frame = requestAnimationFrame(() => {
      timer = window.setTimeout(() => {
        if (!document.hidden) postToNative({
          type: "questionAction", targetId: sessionId, askId, active, action: "autoNext", index: selectedChoice.index,
        });
      }, SELECTION_HOLD_MS);
    });
    return () => {
      cancelAnimationFrame(frame);
      window.clearTimeout(timer);
    };
  }, [selectedChoice, active, askId, sessionId]);

  function send(action: "select" | "tab" | "other" | "next" | "submit" | "cancel", index?: number) {
    setPendingChoice(action === "select" && prompt.auto_advance && index !== undefined
      ? { tab: active, index, label: question.options[index].label } : null);
    postToNative({ type: "questionAction", targetId: sessionId, askId: prompt.ask_id, active, action, ...(index === undefined ? {} : { index }) });
  }

  useLayoutEffect(() => {
    const strip = tabStrip.current;
    const tab = strip?.children[active];
    if (!strip || !tab) return;
    const offset = tab.getBoundingClientRect().left - strip.getBoundingClientRect().left;
    if (offset < 0) strip.scrollLeft += offset;
    else if (offset + tab.clientWidth > strip.clientWidth) strip.scrollLeft += offset + tab.clientWidth - strip.clientWidth;
  }, [active]);

  return (
    <section className="inline-question" aria-labelledby={id}>
      <header>
        <span className="inline-question-title">{t("ask.title")}</span>
        <small>{connected ? t("ask.progress", { done: tabs.filter(tab => tab.answered).length, total: tabs.length }) : t("ask.offline")}</small>
      </header>
      {tabs.length > 1 && <div className="inline-question-tabs" ref={tabStrip}>
        {tabs.map((tab, i) => <button key={i} type="button" aria-current={i === active ? "step" : undefined}
          disabled={submitting} onClick={() => send("tab", i)}>
          {tab.title}{tab.answered && <span aria-hidden="true"> ✓</span>}
        </button>)}
      </div>}
      <h3 id={id}>{question.question || question.header}</h3>
      <div className="inline-question-choices" role="group" aria-labelledby={id}>
        {question.options.map((option, i) => {
          const checked = answer.selected.includes(option.label);
          const descriptionId = id + "-detail-" + i;
          return <button type="button" key={i} aria-label={option.label} aria-pressed={checked}
            aria-describedby={option.description ? descriptionId : undefined} disabled={submitting}
            className={checked ? "chosen" : undefined} onClick={() => send("select", i)}>
            <span className="ask-mark" data-multiple={question.multi_select} aria-hidden="true">
              {checked && question.multi_select && <svg viewBox="0 0 16 16"><path d="m3.5 8 3 3 6-6" /></svg>}
            </span>
            <span>{option.label}{option.description && <small id={descriptionId}>{option.description}</small>}</span>
          </button>;
        })}
        <button type="button" className="inline-question-other" disabled={submitting} onClick={() => send("other")}>
          <svg viewBox="0 0 20 20" aria-hidden="true"><path d="m4 12.5 9-9 3.5 3.5-9 9-4.5 1zM11.5 5l3.5 3.5" /></svg>
          <span>{t("ask.write")}</span>
          {(answer.other ?? "") !== "" && <small>{answer.other}</small>}
        </button>
      </div>
      <footer>
        {confirmCancel ? <>
          <button type="button" className="inline-question-secondary" onClick={() => setConfirmCancel(false)}>{t("ask.keep")}</button>
          <button type="button" className="inline-question-primary" disabled={!connected || submitting}
            onClick={() => { setConfirmCancel(false); send("cancel"); }}>{t("ask.confirmCancel")}</button>
        </> : <>
          <button type="button" className="inline-question-cancel" aria-label={t("ask.cancel")} disabled={!connected || submitting}
            onClick={() => { setPendingChoice(null); setConfirmCancel(true); }}>
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 4 8 8m0-8-8 8" /></svg>
          </button>
          <button type="button" className="inline-question-primary" disabled={!prompt.can_continue}
            onClick={() => send(last ? "submit" : "next")}>
            {t(submitting ? "ask.sending" : last ? "ask.submit" : "ask.next")}
            {!last && !submitting && <span aria-hidden="true"> →</span>}
          </button>
        </>}
      </footer>
    </section>
  );
}
