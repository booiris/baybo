/**
 * Shape of the `/api/benches/:bench/trace` response. The backend reshapes
 * the on-disk `trace.json` (`{session, turns:[{turn, steps}]}`) + the
 * `messages.json` (`{messages}`) into this — the `steps` are exactly the
 * viewer's `ReplayStep`, so they render unchanged.
 */
import type { ReplayStep, SessionMessageRow } from '../types/trace';

/** Loosely-typed turn header from `trace.json`'s `turns[i].turn`. */
export interface BenchTurnMeta {
  id: string;
  session_id?: string;
  status?: unknown;
  kind?: unknown;
  created_at?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
  final_result?: unknown;
}

export interface BenchTurn {
  turn: BenchTurnMeta;
  steps: ReplayStep[];
}

export interface BenchTrace {
  session_id: string | null;
  session_messages: SessionMessageRow[];
  turns: BenchTurn[];
}
