import type { components } from '../api/schema';

type JobStatus = components['schemas']['JobStatus'];

const styles: Record<JobStatus, string> = {
  pending: 'text-ink-soft border-ink-soft',
  in_progress: 'text-info border-info',
  submitted: 'text-info border-info',
  accepted: 'text-info border-info',
  completed: 'text-ok border-ok',
  failed: 'text-err border-err',
  stuck: 'text-warn border-warn',
};

export function JobStatusBadge({ status }: { status: JobStatus }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-bold uppercase border-2 ${styles[status]}`}
    >
      {status.replace('_', ' ')}
    </span>
  );
}
