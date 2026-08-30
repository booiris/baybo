import type { TFunction } from "i18next";
import { memo, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { MarkdownBody, StreamingMarkdownBody } from "./Markdown";
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

export function workedLabel(t: TFunction, elapsedMs?: number, cancelled = false): string {
  const dur = elapsedMs !== undefined && elapsedMs >= 1000 ? formatDuration(t, elapsedMs) : null;
  if (cancelled) return dur === null ? t("chat.cancelled") : t("chat.cancelledFor", { dur });
  return dur === null ? t("chat.worked") : t("chat.workedFor", { dur });
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
function ReasoningStepView({ text, live = false }: { text: string; live?: boolean }) {
  const shown = useSampledText(text);
  // `live` — the run is still being produced into — NOT `shown !== text`: the
  // sampler catches up at every pause in the stream, and a catch-up equality
  // flipped the whole trace to the settled pipeline (KaTeX + highlight + a
  // full subtree remount) for a frame and back on the next delta — a white
  // flash and a math flicker on every hiccup of a long think. The run
  // closing is the one settle that sticks.
  return (
    <div className="step reasoning">
      <span className="step-glyph">✻</span>
      {live ? (
        <StreamingMarkdownBody text={shown} breaks />
      ) : (
        <MarkdownBody text={shown} breaks />
      )}
    </div>
  );
}

/// One MACHINERY step — the reasoning / tool / status / notice traffic the
/// "Worked Xs" summary folds away. `prose` never reaches here: `segmentWorkSteps`
/// routes the model's own words to their own run, outside the collapse.
function WorkStepView({ step, live = false }: { step: WorkStep; live?: boolean }) {
  const { t } = useTranslation();
  switch (step.kind) {
    case "reasoning":
      return <ReasoningStepView text={step.text} live={live} />;
    case "prose":
      return null;
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

/// A run of consecutive steps of one nature. A turn reads as alternating runs:
/// what the agent SAID (speech) and what it DID (machinery).
export type WorkSegment =
  | { kind: "speech"; steps: Extract<WorkStep, { kind: "prose" }>[] }
  | { kind: "machinery"; steps: WorkStep[]; startedAt?: number; endedAt?: number };

/// Split a block's steps into maximal alternating runs of speech (`prose` — the
/// model's own words mid-turn) and machinery (reasoning / tool / status /
/// notice).
///
/// This is the whole redesign, and it is a pure projection: the step list is
/// unchanged, only what the collapse is allowed to hide changes. Speech renders
/// at ANSWER typography (1rem Inter, `.msg.assistant`'s reading band) in
/// document flow, so when a tool call interrupts the model mid-sentence and
/// `foldStreamingIntoProse` moves the text into the block, it lands looking
/// exactly like it did a frame earlier — no jump, no 1rem→0.85rem shrink. The
/// fold stops being felt instead of merely being softened, and "Worked Xs ›"
/// comes to mean what a reader expects: the machinery is hidden, the words
/// are not.
///
/// Order-preserving, so the live view and a cold reload — both derived from the
/// same ordered `steps[]` — agree by construction. Mirrors the web chat's
/// `segmentWorkSteps`.
export function segmentWorkSteps(
  steps: WorkStep[],
  workStartedAt?: number,
  workEndedAt?: number,
): WorkSegment[] {
  const out: WorkSegment[] = [];
  for (const s of steps) {
    const tail = out.length > 0 ? out[out.length - 1] : undefined;
    if (s.kind === "prose") {
      if (tail !== undefined && tail.kind === "speech") tail.steps.push(s);
      else out.push({ kind: "speech", steps: [s] });
    } else if (tail !== undefined && tail.kind === "machinery") tail.steps.push(s);
    else out.push({ kind: "machinery", steps: [s] });
  }
  // Each machinery run is bounded by the remarks around it: it starts when the
  // model last spoke (or when the turn did) and ends when it speaks next (or
  // when the turn did). The runs therefore TILE the turn — the ladder's
  // durations add up to the whole — and each reads as "how long it worked
  // before saying this", which is what its header claims.
  const proseAt = (seg: WorkSegment | undefined, which: "first" | "last"): number | undefined => {
    if (seg === undefined || seg.kind !== "speech") return undefined;
    const step = which === "first" ? seg.steps[0] : seg.steps[seg.steps.length - 1];
    return step.at;
  };
  return out.map((seg, i) =>
    seg.kind !== "machinery"
      ? seg
      : {
          ...seg,
          startedAt: proseAt(out[i - 1], "last") ?? (i === 0 ? workStartedAt : undefined),
          endedAt: proseAt(out[i + 1], "first") ?? (i === out.length - 1 ? workEndedAt : undefined),
        },
  );
}

/// A machinery run's collapsed label: the duration it actually covers when both
/// bounds are known, else a bare "Worked" — inventing a number from the block's
/// total would be a lie the reader cannot detect, and a STEP COUNT in that slot
/// answers a question nobody asked (it is what the run expands to show).
///
/// `wholeBlockMs` is not an invention: when a run is the block's ONLY segment
/// its span IS the block's `elapsedMs` by definition. That matters because
/// `elapsedMs` outlives the anchor — the mirror restore drops `startedAt` on a
/// block still active at persist (`sanitizeRestoredRows`), and rows written
/// before that narrowed lost theirs on disk — so without it a restored turn
/// loses a duration it still holds.
///
/// Diverges from the web chat's `workRunLabel`, which keeps the count: only this
/// surface has a mirror that can strip the anchor, so only here is the fallback
/// common enough to be worth reading well.
export function workRunLabel(
  t: TFunction,
  seg: WorkSegment,
  cancelled = false,
  wholeBlockMs?: number,
): string {
  const startedAt = seg.kind === "machinery" ? seg.startedAt : undefined;
  const endedAt = seg.kind === "machinery" ? seg.endedAt : undefined;
  const span =
    startedAt === undefined || endedAt === undefined || endedAt < startedAt
      ? wholeBlockMs
      : endedAt - startedAt;
  return workedLabel(t, span, cancelled);
}

/// One turn's work block: while the turn runs, a spinner header over the live
/// feed; once closed, a dim "Worked Xs ›" summary the user can tap open,
/// followed by a hairline divider (the web chat's shape, restyled to the mobile
/// line-minimal system).
///
/// Mid-turn narration is NOT part of what the summary hides — it renders in
/// every state, at reading weight, in its true chronological position between
/// the machinery runs it interrupted.
/// One run of machinery — the work between two of the model's remarks — with
/// its OWN header and its OWN expansion. Per-run on both counts: the header
/// answers "how long did it work before saying this", which a turn-level total
/// cannot; and opening one run of a long turn must not insert every other run's
/// lines under the reader's thumb.
function WorkRunView({
  seg,
  live,
  cancelled,
  wholeBlockMs,
  defaultExpanded,
  onToggle,
}: {
  seg: Extract<WorkSegment, { kind: "machinery" }>;
  live: boolean;
  cancelled: boolean;
  /// Set only when this run is the block's ONLY segment, so its span is the
  /// block's own `elapsedMs` — see `workRunLabel`.
  wholeBlockMs?: number;
  /// Read-only transcripts open an unanswered tail so its last visible thing
  /// is the work itself, not a misleadingly final-looking `Worked` summary.
  defaultExpanded?: boolean;
  onToggle?: (expanded: boolean) => void;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(defaultExpanded ?? false);
  if (live) {
    return (
      <div className="work active">
        <div className="work-head">
          <span className="work-spin">✻</span>
          <span>{t("chat.working")}</span>
          <LiveElapsed startedAt={seg.startedAt} />
        </div>
        {seg.steps.length > 0 && (
          <div className="work-steps">
            {seg.steps.map((s, j) => (
              <WorkStepView key={j} step={s} live />
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
        <span>{workRunLabel(t, seg, cancelled, wholeBlockMs)}</span>
        <span className={`work-chevron${expanded ? " open" : ""}`}>›</span>
      </button>
      {expanded && (
        <div className="work-steps">
          {seg.steps.map((s, j) => (
            <WorkStepView key={j} step={s} />
          ))}
        </div>
      )}
    </div>
  );
}

export const WorkBlockView = memo(function WorkBlockView({
  row,
  defaultExpanded,
  onToggle,
}: {
  row: WorkRow;
  /// Initial state only: a later final answer must not snap shut work the
  /// reader was already looking at, and a manual collapse remains respected.
  defaultExpanded?: boolean;
  /// Fired with the NEW expanded state on a user tap, so the transcript can
  /// disengage follow-to-bottom when a run opens — otherwise the pin chases
  /// the newest edge and the inserted steps shove the summary UP instead of
  /// opening downward from it.
  onToggle?: (expanded: boolean) => void;
}) {
  const segments = segmentWorkSteps(row.steps, row.startedAt, row.elapsedMs !== undefined && row.startedAt !== undefined ? row.startedAt + row.elapsedMs : undefined);
  const lastMachinery = segments.reduce((at, seg, i) => (seg.kind === "machinery" ? i : at), -1);
  // A live turn with no step yet still needs its "处理中" affordance and has no
  // run to hang it on — synthesize the empty one.
  const runs: WorkSegment[] =
    segments.length === 0 && row.active
      ? [{ kind: "machinery", steps: [], startedAt: row.startedAt }]
      : segments;
  return (
    <div className="work-ladder" data-row-id={row.id}>
      {runs.map((seg, i) =>
        seg.kind === "speech" ? (
          <div className="work-said" key={`s${i}`}>
            {seg.steps.map((s, j) => (
              <MarkdownBody key={j} text={s.text} />
            ))}
          </div>
        ) : (
          <WorkRunView
            key={`m${i}`}
            seg={seg}
            // Only the turn's LAST run is still being produced into; the ones
            // above it are finished and collapse like any other.
            live={row.active && (i === lastMachinery || segments.length === 0)}
            cancelled={row.cancelled === true && i === lastMachinery}
            // A lone machinery run spans the whole turn, so the block's own
            // duration is exactly its span — the only number that survives the
            // mirror dropping `startedAt`. With a remark in the turn the runs
            // are shorter than the block and there is nothing honest to fall
            // back to, so those keep the step count.
            wholeBlockMs={runs.length === 1 ? row.elapsedMs : undefined}
            defaultExpanded={defaultExpanded}
            onToggle={onToggle}
          />
        ),
      )}
      {!row.active && <div className="work-divider" />}
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
