import type { Agent, IssueRun } from './boardModel';

export function workingAgentIds(activeRuns: IssueRun[]): Set<string> {
  return new Set(
    activeRuns.filter((run) => run.status === 'running').map((run) => run.agent_id),
  );
}

/// Agents with a run waiting on a free slot, and none of their own going.
///
/// Separate from [`workingAgentIds`] rather than a second status on one map:
/// an agent can have a queued run on one card while working another, and the
/// strip must show that as working — the dimmed queued face means "this
/// teammate is idle *and* has something waiting", which is a different thing
/// to see.
export function queuedAgentIds(activeRuns: IssueRun[]): Set<string> {
  const working = workingAgentIds(activeRuns);
  return new Set(
    activeRuns
      .filter((run) => run.status === 'queued' && !working.has(run.agent_id))
      .map((run) => run.agent_id),
  );
}

export function handleOf(team: Agent[], agentId: string): string {
  return team.find((agent) => agent.id === agentId)?.handle ?? agentId;
}

/// What an unpinned `llm` reads as — this agent follows whatever
/// `default-llm` is at the time.
///
/// Not the default's own name, which would be the shorter answer: `GET
/// /v1/llm/models` lists every configured entry and the default is one of
/// them, so the name would appear twice in one picker meaning two different
/// things — follow the default wherever it moves, or freeze to the model that
/// happens to be default today.
export const UNPINNED_LLM = '—';

/// Longest handle the grammar accepts. Mirrors `MAX_AGENT_HANDLE_CHARS`.
const MAX_HANDLE_CHARS = 32;


/// Why this name cannot be an agent's, or null when it can.
///
/// A name **is** a handle — the server's `AgentHandle::parse`, mirrored here
/// so the form can refuse before the round trip rather than after it. The
/// server is still the judge: it also settles collisions, which no client can
/// know about.
export function handleProblem(name: string): string | null {
  const value = name.trim();
  if (value === '') return null;
  if (!/^[a-z]/.test(value)) return 'has to start with a lowercase letter';
  if (!/^[a-z0-9-]*$/.test(value)) return 'lowercase letters, digits and “-” only';
  if (value.endsWith('-')) return 'cannot end with “-”';
  if (value.length > MAX_HANDLE_CHARS) return `at most ${MAX_HANDLE_CHARS} characters`;
  return null;
}
