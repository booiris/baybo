import type { Agent, IssueRun } from './boardModel';

/**
 * Which agents are executing something right now.
 *
 * Derived from the board's active-run list rather than read off the roster,
 * so the dot beside a handle and the shimmer on a card are the same fact.
 * A *queued* run is not working — nothing has started, and showing it as
 * busy would make the strip claim work that has not begun.
 */
export function workingAgentIds(activeRuns: IssueRun[]): Set<string> {
  return new Set(
    activeRuns.filter((run) => run.status === 'running').map((run) => run.agent_id),
  );
}

/**
 * Whoever an issue names, even after they have left the team.
 *
 * A removed agent keeps its row so past work still resolves, but it is not
 * in the roster — so a card assigned to one has to render *something*, and
 * the raw id beats a blank. Callers show the returned handle with an `@`.
 */
export function handleOf(team: Agent[], agentId: string): string {
  return team.find((agent) => agent.id === agentId)?.handle ?? agentId;
}
