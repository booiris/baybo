/**
 * The job column — a document-outline-style list of the trace's jobs that
 * doubles as jump-anchors. Each entry previews the user input that started the
 * job, so you can tell the jobs apart by what was asked rather than by number.
 * Clicking one selects the job and scrolls the tree to it (`data-job-id`).
 */
import type { JobTrace, SessionMessageRow, TraceOverview } from '../../types/trace';
import { formatDuration, formatTok, jobDurationMs, jobInputText, summaryTokens } from './traceFormat';
import { jobRollup } from './traceTreeModel';
import { scrollToAnchor } from './scrollToAnchor';

export function JobAnchors({
  overview,
  jobTraces,
  messageLog,
  activeJobId,
  onSelectJob,
}: {
  overview: TraceOverview;
  jobTraces: Map<string, JobTrace>;
  messageLog: SessionMessageRow[];
  activeJobId: string;
  onSelectJob: (jobId: string) => void;
}) {
  const jump = (jobId: string) => {
    onSelectJob(jobId);
    scrollToAnchor(`[data-job-id="${jobId}"]`);
  };

  if (overview.jobs.length <= 1) return null;

  return (
    <div className="shrink-0 w-56 border-r-2 border-black bg-canvas flex flex-col overflow-y-auto z-10">
      <div className="px-2 py-1.5 border-b border-black/20 text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        jobs · {overview.jobs.length}
      </div>
      {overview.jobs.map((j, i) => {
        const trace = jobTraces.get(j.job_id);
        const rollup = jobRollup(j, trace);
        const active = j.job_id === activeJobId;
        const tokens = summaryTokens(j);
        const dur = jobDurationMs(j, trace);
        const input = trace ? jobInputText(trace, messageLog) : null;

        return (
          <button
            key={j.job_id}
            type="button"
            onClick={() => jump(j.job_id)}
            title={input ?? `Job #${i + 1} · ${j.job_status_kind}`}
            className={`px-2 py-2 text-left border-b border-black/10 cursor-pointer transition-colors border-l-[3px] ${
              active
                ? 'bg-selected text-ink border-l-black'
                : 'border-l-transparent hover:bg-gray-100 text-ink'
            }`}
          >
            <div className="flex items-center gap-1.5 min-w-0">
              <span className="font-mono text-[0.75rem] font-bold shrink-0">#{i + 1}</span>
              <span className="font-mono text-[0.62rem] text-ink-soft truncate">{j.job_status_kind}</span>
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
              ↑{formatTok(tokens.inputTotal)} ↓{formatTok(tokens.output)} · {formatDuration(dur)}
            </div>
          </button>
        );
      })}
    </div>
  );
}
