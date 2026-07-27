import type { TFunction } from "i18next";
import { memo, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { MarkdownBody } from "./Markdown";
import type { WorkRow, WorkStep } from "./types";

/// Humanized duration: seconds under a minute, "Xm Ys" under an hour,
/// "Xh Ym" beyond (seconds dropped at hour scale).
export function formatDuration(t: TFunction, ms: number): string {
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

export function workedLabel(t: TFunction, elapsedMs?: number): string {
  if (elapsedMs !== undefined && elapsedMs >= 1000) {
    return t("chat.workedFor", { dur: formatDuration(t, elapsedMs) });
  }
  return t("chat.worked");
}

/// The step's approval label: "waiting for approval" while the gate holds the
/// call, then the user's verdict — which persists (`ToolResultMeta::approval`),
/// so it is still there after a reload. `null` for a call that never prompted,
/// which is the overwhelming majority.
export function approvalLabel(t: TFunction, step: WorkStep): string | null {
  if (step.kind !== "tool") return null;
  if (step.awaitingApproval) return t("chat.approvalWaiting");
  switch (step.approval) {
    case "approve":
      return t("chat.approvalApproved");
    case "approve_always":
      return t("chat.approvalApprovedAlways");
    case "deny":
      return t("chat.approvalDenied");
    default:
      return null;
  }
}

/// How often a still-growing reasoning trace is re-parsed. ~7 updates a second
/// still reads as live in a dim subordinate panel; see `useSampledText`.
const REASONING_SAMPLE_MS = 150;

/// The latest `text`, but re-published at most once per `REASONING_SAMPLE_MS`.
///
/// A live reasoning step merges each `reasoning` frame into ONE trailing step
/// (see `Transcript.tsx`), so its text grows monotonically and `MarkdownBody`'s
/// memo misses on every frame — the whole accumulated trace is re-parsed per
/// frame, making a turn's markdown cost quadratic in its length (measured in
/// jsdom, one frame per 8 chars: 4k chars ≈ 1.0s, 8k ≈ 3.8s, 16k ≈ 15.7s of
/// synchronous main-thread work, and this runs on a phone's WKWebView). Nothing
/// upstream paces it: a provider reasoning delta is one wire frame is one push,
/// and unlike `appendStreaming` — which is rAF-coalesced — this path had no
/// limiter at all. The trailing timer guarantees the final text lands once the
/// step stops growing, so no trace is left truncated.
function useSampledText(text: string): string {
  const [shown, setShown] = useState(text);
  const shownAtRef = useRef(0);
  useEffect(() => {
    const due = shownAtRef.current + REASONING_SAMPLE_MS - Date.now();
    if (due <= 0) {
      shownAtRef.current = Date.now();
      setShown(text);
      return;
    }
    const id = setTimeout(() => {
      shownAtRef.current = Date.now();
      setShown(text);
    }, due);
    return () => clearTimeout(id);
  }, [text]);
  return shown;
}

/// Reasoning is the model's own markdown, rendered as prose beside the ✻ marker
/// — as raw text a `**要点：**` reached the reader as literal asterisks.
function ReasoningStepView({ text }: { text: string }) {
  const shown = useSampledText(text);
  return (
    <div className="step reasoning">
      <span className="step-glyph">✻</span>
      <MarkdownBody text={shown} breaks />
    </div>
  );
}

function WorkStepView({ step }: { step: WorkStep }) {
  const { t } = useTranslation();
  switch (step.kind) {
    case "reasoning":
      return <ReasoningStepView text={step.text} />;
    case "prose":
      return (
        <div className="step prose">
          <MarkdownBody text={step.text} />
        </div>
      );
    case "status":
      return <div className="step status">{step.text}</div>;
    case "notice":
      // An out-of-band notice that landed mid-turn — a leveled line inside the
      // block, so it doesn't sever the card. Severity reads via the color ramp
      // (info dim → warn ink → error red); △ is text-presentation (no emoji).
      return (
        <div className={`step notice notice-${step.level}`}>
          <span className="step-glyph">△</span>
          <span className="step-text">{step.text}</span>
        </div>
      );
    case "tool": {
      const failed = step.status === "error" || step.status === "denied";
      // A call waiting on the user is NOT "running" — the pulse would read as
      // work in progress when nothing is happening until the card is answered.
      const awaiting = Boolean(step.awaitingApproval);
      const running = step.status === "running" && !awaiting;
      const approval = approvalLabel(t, step);
      return (
        <div
          className={`step tool${failed ? " failed" : ""}${running ? " running" : ""}${awaiting ? " awaiting" : ""}`}
        >
          {/* U+25CF, not U+23FA — the latter takes iOS's blue emoji presentation. */}
          <span className="step-glyph">●</span>
          <span className="step-text">
            {step.label}
            {step.summary ? ` — ${step.summary}` : ""}
            {approval ? <span className="step-approval">{approval}</span> : null}
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

/// Line shown for a turn-phase `status` frame. A ternary rather than a switch
/// on purpose: `wire.test.ts` scrapes `case "…":` labels out of Transcript.tsx
/// to prove the frame router handles exactly the declared kinds, and a stray
/// switch anywhere it reads would be counted as a routed frame.
export function compactionStatusText(t: TFunction, phase: string): string {
  return phase === "compacting"
    ? t("chat.compacting")
    : phase === "compacted"
      ? t("chat.compacted")
      : phase;
}
