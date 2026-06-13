/**
 * Shape of the `/api/benches/:bench/trace` response. The backend reshapes
 * the on-disk `trace.json` (`{session, jobs:[{job, steps}]}`) + the
 * `messages.json` (`{messages}`) into this — the `steps` are exactly the
 * viewer's `ReplayStep`, so they render unchanged.
 */
import type { ReplayStep, SessionMessageRow } from '../types/trace';

/** Loosely-typed job header from `trace.json`'s `jobs[i].job`. */
export interface BenchJobMeta {
  id: string;
  session_id?: string;
  status?: unknown;
  kind?: unknown;
  created_at?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
  final_result?: unknown;
}

export interface BenchJob {
  job: BenchJobMeta;
  steps: ReplayStep[];
}

export interface BenchTrace {
  session_id: string | null;
  session_messages: SessionMessageRow[];
  jobs: BenchJob[];
}
