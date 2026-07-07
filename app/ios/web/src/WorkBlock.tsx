import type { TFunction } from "i18next";
import { memo, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { MarkdownBody } from "./Markdown";
import type { WorkRow, WorkStep } from "./types";

/// Humanized duration: seconds under a minute, "Xm Ys" under an hour,
/// "Xh Ym" beyond (seconds dropped at hour scale).
function formatDuration(t: TFunction, ms: number): string {
  const total = Math.round(ms / 1000);
  if (total < 60) return t("chat.durS", { s: total });
  if (total < 3600) {
    const m = Math.floor(total / 60);
    const s = total % 60;
    return s > 0 ? t("chat.durMS", { m, s }) : t("chat.durM", { m });
  }
  let h = Math.floor(total / 3600);
  let m = Math.round((total % 3600) / 60);
  if (m === 60) {
    h += 1;
    m = 0;
  }
  return m > 0 ? t("chat.durHM", { h, m }) : t("chat.durH", { h });
}

/// Live elapsed counter for an open work block; suppressed below 1s so quick
/// turns don't flash "0s".
function LiveElapsed({ startedAt }: { startedAt?: number }) {
  const { t } = useTranslation();
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  if (startedAt === undefined) return null;
  const ms = now - startedAt;
  if (ms < 1000) return null;
  return <span className="work-elapsed">{formatDuration(t, ms)}</span>;
}

function workedLabel(t: TFunction, elapsedMs?: number): string {
  if (elapsedMs !== undefined && elapsedMs >= 1000) {
    return t("chat.workedFor", { dur: formatDuration(t, elapsedMs) });
  }
  return t("chat.worked");
}

function WorkStepView({ step }: { step: WorkStep }) {
  switch (step.kind) {
    case "reasoning":
      return (
        <div className="step reasoning">
          <span className="step-glyph">✻</span>
          <MarkdownBody text={step.text} />
        </div>
      );
    case "prose":
      return (
        <div className="step prose">
          <MarkdownBody text={step.text} />
        </div>
      );
    case "status":
      return <div className="step status">{step.text}</div>;
    case "tool": {
      const failed = step.status === "error" || step.status === "denied";
      const running = step.status === "running";
      return (
        <div className={`step tool${failed ? " failed" : ""}${running ? " running" : ""}`}>
          {/* U+25CF, not U+23FA — the latter takes iOS's blue emoji presentation. */}
          <span className="step-glyph">●</span>
          <span className="step-text">
            {step.label}
            {step.summary ? ` — ${step.summary}` : ""}
          </span>
        </div>
      );
    }
  }
}

/// One turn's work block: while the turn runs, a soft card with a spinner
/// header and the live step feed; once closed, a dim "Worked Xs ›" summary the
/// user can tap open, followed by a hairline divider (the web chat's shape,
/// restyled to the mobile line-minimal system).
export const WorkBlockView = memo(function WorkBlockView({
  row,
  onToggle,
}: {
  row: WorkRow;
  /// Fired with the NEW expanded state on a user tap, so the transcript can
  /// disengage follow-to-bottom when a block opens — otherwise the pin chases
  /// the newest edge and the inserted steps shove the summary UP instead of
  /// opening downward from it.
  onToggle?: (expanded: boolean) => void;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);

  if (row.active) {
    return (
      <div className="work active">
        <div className="work-head">
          <span className="work-spin">✻</span>
          <span>{t("chat.working")}</span>
          <LiveElapsed startedAt={row.startedAt} />
        </div>
        {row.steps.length > 0 && (
          <div className="work-steps">
            {row.steps.map((s, i) => (
              <WorkStepView key={i} step={s} />
            ))}
          </div>
        )}
      </div>
    );
  }
  return (
    <div className="work closed">
      <button
        type="button"
        className="work-summary"
        onClick={() => {
          const next = !expanded;
          setExpanded(next);
          onToggle?.(next);
        }}
      >
        <span>{workedLabel(t, row.elapsedMs)}</span>
        <span className={`work-chevron${expanded ? " open" : ""}`}>›</span>
      </button>
      {expanded && (
        <div className="work-steps">
          {row.steps.map((s, i) => (
            <WorkStepView key={i} step={s} />
          ))}
        </div>
      )}
      <div className="work-divider" />
    </div>
  );
});
