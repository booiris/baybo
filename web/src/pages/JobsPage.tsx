import { useCallback, useEffect, useState } from 'react';
import { RiRefreshLine } from 'react-icons/ri';
import { Button } from '../components/Button';
import { JobStatusBadge } from '../components/JobStatusBadge';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';

type Job = components['schemas']['Job'];
type JobStatus = components['schemas']['JobStatus'];

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  return {
    date: d.toISOString().slice(0, 10),
    time: d.toISOString().slice(11, 23),
  };
}

function kindLabel(kind: Job['kind']): string {
  switch (kind.type) {
    case 'llm_call':
      return `llm: ${kind.model}`;
    case 'tool_execution':
      return `tool: ${kind.tool_name}`;
    case 'skill_execution':
      return `skill: ${kind.skill_name}`;
    case 'cron_execution':
      return `cron: ${kind.cron_job_id}`;
    case 'context_compression':
      return `compress: ${kind.strategy}`;
    case 'memory_operation':
      return `memory: ${kind.operation}`;
    case 'user_message_handling':
      return `msg: ${kind.session_id}`;
  }
}

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black';

export function JobsPage() {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [jobs, setJobs] = useState<Job[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    const { data, error: apiError, response } = await client.GET('/v1/jobs');
    setLoading(false);
    if (apiError) {
      setError(apiError.error);
      if (response.status === 401) logout();
      return;
    }
    setJobs(data?.items ?? []);
  }, [client, logout]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="p-8">
      <div className="flex justify-between items-start mb-8">
        <div>
          <h2 className="text-[2.5rem] font-bold uppercase -tracking-[0.05em] mb-2">JOBS</h2>
          <p className="text-ink-soft">Async operations tracked by the gateway.</p>
        </div>
        <Button onClick={() => void load()} disabled={loading}>
          <RiRefreshLine /> Refresh
        </Button>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="bg-white border-[3px] border-black rounded-md shadow-brutal flex flex-col overflow-hidden">
        <table className="w-full border-collapse">
          <thead>
            <tr>
              <th className={thCell}>Created</th>
              <th className={thCell}>Kind</th>
              <th className={thCell}>Status</th>
              <th className={thCell}>Session</th>
              <th className={thCell}>Id</th>
            </tr>
          </thead>
          <tbody>
            {jobs?.map((job, idx) => {
              const { date, time } = splitTimestamp(job.created_at);
              const notLast = idx !== jobs.length - 1;
              const cell = `px-6 py-4 ${notLast ? 'border-b border-black' : ''}`;
              return (
                <tr key={job.id} className="hover:bg-gray-50">
                  <td className={cell}>
                    <div className="text-ink-soft text-[0.85rem] leading-snug">
                      {date}
                      <br />
                      {time}
                    </div>
                  </td>
                  <td className={cell}>
                    <code className="font-mono bg-gray-100 px-2 py-1 rounded text-[0.9rem]">
                      {kindLabel(job.kind)}
                    </code>
                  </td>
                  <td className={cell}>
                    <JobStatusBadge status={job.status as JobStatus} />
                  </td>
                  <td className={cell}>
                    <code className="font-mono text-[0.85rem]">{job.session_id}</code>
                  </td>
                  <td className={cell}>
                    <code className="font-mono text-[0.85rem] text-ink-soft">{job.id}</code>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>

        <div className="flex justify-between items-center px-6 py-4 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft">
            {jobs ? `Showing ${jobs.length} jobs` : loading ? 'Loading...' : 'No data'}
          </span>
        </div>
      </div>
    </div>
  );
}
