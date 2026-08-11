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

export type ModelPool = { names: string[]; defaultName: string } | null;

/// The llm picker's rows: one per configured model, no more.
///
/// The default's row carries the **empty value** — that is, picking it pins
/// nothing and the agent follows `default-llm` wherever it moves. There is
/// deliberately no way to pin the model that is default today: the two are
/// indistinguishable until the default moves, and a picker listing `deepseek`
/// twice — once as itself, once as "default" — is a worse thing to read than
/// the distinction is worth keeping.
export function llmOptions(pool: ModelPool): { value: string; label: string }[] {
  if (pool === null) return [];
  return pool.names.map((name) => ({
    value: name === pool.defaultName ? '' : name,
    label: name,
  }));
}

/// Which row is showing, for an agent's stored pin.
///
/// An agent pinned to the model that is *currently* default shows as that
/// model's row, which is the unpinned one — the only row it could show as,
/// now that there is one row per model.
export function llmSelected(pinned: string | null | undefined, pool: ModelPool): string {
  if (pinned == null || pinned === '') return '';
  return pool !== null && pinned === pool.defaultName ? '' : pinned;
}

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
