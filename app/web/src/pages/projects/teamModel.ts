import { runState, type Agent, type IssueRun } from './boardModel';
import type { AvatarRun } from './Avatar';

/// Where each teammate stands, over every run on the board — one face per
/// agent, so one answer per agent.
///
/// The rank is the whole idea, because an agent can be on three cards at
/// once: **working** outranks the rest, since a teammate who is actually
/// going is not idle whatever else is stacked behind them; and among the
/// waits, **held** outranks **queued**, because a queued run starts on its
/// own when a slot frees and a held one does not start until somebody raises
/// a ceiling. The dimmed face means "idle *and* something is waiting", which
/// is a different thing to see from plainly idle.
///
/// A held run used to answer to neither of the two sets this replaced, so a
/// board whose whole team the budget had stopped drew a full strip of grey
/// idle dots with nothing anywhere to say why.
const RANK: Record<Exclude<AvatarRun, null>, number> = { running: 3, held: 2, queued: 1 };

export function agentRunStates(activeRuns: IssueRun[]): Map<string, AvatarRun> {
  const states = new Map<string, AvatarRun>();
  for (const run of activeRuns) {
    const state = runState(run.status);
    if (state === null) continue;
    const seen = states.get(run.agent_id);
    if (seen == null || RANK[state] > RANK[seen]) states.set(run.agent_id, state);
  }
  return states;
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
