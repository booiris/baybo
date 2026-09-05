/**
 * The turn column — a document-outline-style list of the trace's turns that
 * doubles as jump-anchors. Each entry previews the user input that started the
 * turn, so you can tell the turns apart by what was asked rather than by number.
 * Clicking one selects the turn and scrolls the tree to it (`data-turn-id`).
 */
import type { SessionMessageRow, TraceOverview, TurnTrace } from '../../types/trace';
import {
  formatDuration,
  formatTok,
  transcriptInputText,
  turnDurationMs,
  turnInputText,
  turnTokens,
} from './traceFormat';
import { partitionTranscript, turnLabels, turnRollup } from './traceTreeModel';
import { scrollToAnchor } from './scrollToAnchor';

export function TurnAnchors({
  overview,
  turnTraces,
  messageLog,
  activeTurnId,
  onSelectTurn,
}: {
  overview: TraceOverview;
  turnTraces: Map<string, TurnTrace>;
  messageLog: SessionMessageRow[];
  activeTurnId: string;
  onSelectTurn: (turnId: string) => void;
}) {
  const jump = (turnId: string) => {
    onSelectTurn(turnId);
    scrollToAnchor(`[data-turn-id="${turnId}"]`);
  };

  const labels = turnLabels(overview.turns);
  const transcripts = partitionTranscript(messageLog, overview.turns);

  if (overview.turns.length <= 1) return null;

  return (
    <div className="shrink-0 w-56 border-r-2 border-black bg-canvas flex flex-col overflow-y-auto z-10">
      <div className="px-2 py-1.5 border-b border-black/20 text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        turns · {overview.turns.length}
      </div>
      {overview.turns.map((j, i) => {
        const trace = turnTraces.get(j.turn_id);
        const rollup = turnRollup(j, trace);
        const active = j.turn_id === activeTurnId;
        const tokens = turnTokens(j, trace);
        const dur = turnDurationMs(j, trace);
        // A stepless turn (external agent) has no LLM span to read the prompt
        // off, so fall back to its own slice of the transcript — otherwise the
        // whole anchor column of an external session reads "no user input
        // recorded" over transcripts that open with exactly that prompt.
        const input =
          trace && trace.steps.length > 0
            ? turnInputText(trace, messageLog)
            : transcriptInputText(transcripts.get(j.turn_id) ?? []);

        return (
          <button
            key={j.turn_id}
            type="button"
            onClick={() => jump(j.turn_id)}
            title={input ?? `${labels[i].long} · ${j.turn_status_kind}`}
            className={`px-2 py-2 text-left border-b border-black/10 cursor-pointer transition-colors border-l-[3px] ${
              active
                ? 'bg-selected text-ink border-l-black'
                : 'border-l-transparent hover:bg-gray-100 text-ink'
            }`}
          >
            <div className="flex items-center gap-1.5 min-w-0">
              <span className="font-mono text-[0.75rem] font-bold shrink-0">{labels[i].short}</span>
              {labels[i].kind != null && (
                <span className="font-mono text-[0.62rem] font-bold text-ink-soft shrink-0">{labels[i].kind}</span>
              )}
              <span className="font-mono text-[0.62rem] text-ink-soft truncate">{j.turn_status_kind}</span>
              {rollup.hasFailure && <span className="ml-auto shrink-0 w-1.5 h-1.5 rounded-full bg-err" />}
            </div>
            <div
              className={`mt-1 text-[0.68rem] leading-snug line-clamp-2 ${
                input != null ? 'text-ink' : 'text-ink-soft italic'
              }`}
            >
              {input ?? (trace ? 'no user input recorded' : 'loading…')}
            </div>
            <div className="mt-1 font-mono text-[0.6rem] text-ink-soft">
              ↑{formatTok(tokens.input)} ↓{formatTok(tokens.output)} · {formatDuration(dur)}
            </div>
          </button>
        );
      })}
    </div>
  );
}
