import { useRef } from "react";
import { useTranslation } from "react-i18next";
import type { RestWorkStep } from "./types";

export function QuestionResult({
  ask,
  onToggle,
}: {
  ask: NonNullable<RestWorkStep["ask"]>;
  onToggle?: (expanded: boolean) => void;
}) {
  const { t } = useTranslation();
  const details = useRef<HTMLDetailsElement>(null);
  const questions = ask.questions ?? [];
  const count = questions.filter((q) => q.skipped !== true).length;
  const closed = ask.cancelled === true || ask.declined;
  const title =
    ask.cancelled === true
      ? t("ask.cancelled")
      : ask.declined
        ? t("ask.declined")
        : t("ask.answered", { count });
  if (questions.length === 0) return <div className="ask-result">{title}</div>;

  return (
    <details ref={details} className="ask-result">
      {/* Release follow-to-bottom before the native disclosure grows and triggers ResizeObserver. */}
      <summary onClick={() => onToggle?.(details.current?.open === false)}>{title}</summary>
      {questions.map((q) => {
        const selected = q.selected ?? [];
        const options = q.options ?? [];
        const other = q.other ?? "";
        // Older records can retain an answer without the original option list.
        const choices = [
          ...options,
          ...selected
            .filter((label) => !options.some((o) => o.label === label))
            .map((label) => ({ label, description: "" })),
        ];
        return (
          <section key={q.header}>
            {q.header && q.header !== q.question && (
              <div className="ask-topic">{q.header}</div>
            )}
            <h4>{q.question || q.header}</h4>
            {choices.length > 0 && (
              <ul>
                {choices.map((option) => {
                  const checked = selected.includes(option.label);
                  return (
                    <li
                      key={option.label}
                      className={checked ? "chosen" : undefined}
                    >
                      <span
                        className="ask-mark"
                        data-multiple={q.multi_select === true}
                        role="img"
                        aria-label={t(
                          checked ? "ask.selected" : "ask.unselected",
                        )}
                      >
                        {checked && (
                          <svg viewBox="0 0 16 16" aria-hidden="true">
                            <path d="m3.5 8 3 3 6-6" />
                          </svg>
                        )}
                      </span>
                      <div>
                        {option.label}
                        {option.description && (
                          <small>{option.description}</small>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            )}
            {other.length > 0 && (
              <div className="ask-written">
                <small>{t("ask.other")}</small>
                {other}
              </div>
            )}
            {!closed &&
              (q.skipped === true ||
                (selected.length === 0 && other.length === 0)) && (
                <small>
                  {t(q.skipped === true ? "ask.skipped" : "ask.noAnswer")}
                </small>
              )}
          </section>
        );
      })}
    </details>
  );
}
