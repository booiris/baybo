/**
 * A slim left rail of job jump-anchors — a document-outline-style TOC over the
 * trace's jobs. Clicking one selects the job and scrolls the tree to it
 * (`data-job-id`). A red dot marks jobs whose subtree contains a failure.
 */
import type { JobTrace, TraceOverview } from '../../types/trace';
import { jobRollup } from './traceTreeModel';
import { scrollToAnchor } from './scrollToAnchor';

export function JobAnchors({
  overview,
  jobTraces,
  activeJobId,
  onSelectJob,
}: {
  overview: TraceOverview;
  jobTraces: Map<string, JobTrace>;
  activeJobId: string;
  onSelectJob: (jobId: string) => void;
}) {
  const jump = (jobId: string) => {
    onSelectJob(jobId);
    scrollToAnchor(`[data-job-id="${jobId}"]`);
  };

  if (overview.jobs.length <= 1) return null;

  return (
    <div className="shrink-0 w-12 border-r-2 border-black bg-canvas flex flex-col overflow-y-auto z-10">
      <div className="px-1 py-1.5 border-b border-black/20 text-[0.5rem] font-bold uppercase tracking-wider text-ink-soft text-center">
        jobs
      </div>
      {overview.jobs.map((j, i) => {
        const rollup = jobRollup(j, jobTraces.get(j.job_id));
        const active = j.job_id === activeJobId;
        return (
          <button
            key={j.job_id}
            type="button"
            onClick={() => jump(j.job_id)}
            title={`Job #${i + 1} · ${j.job_status_kind}`}
            className={`relative px-1 py-1.5 text-center border-b border-black/10 cursor-pointer font-mono text-[0.72rem] font-bold transition-colors ${
              active ? 'bg-selected text-ink' : 'text-ink-soft hover:bg-gray-100'
            }`}
          >
            #{i + 1}
            {rollup.hasFailure && <span className="absolute top-1 right-1 w-1.5 h-1.5 rounded-full bg-err" />}
          </button>
        );
      })}
    </div>
  );
}
