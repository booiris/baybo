import type { Agent, IssueRun } from './boardModel';

export function workingAgentIds(activeRuns: IssueRun[]): Set<string> {
  return new Set(
    activeRuns.filter((run) => run.status === 'running').map((run) => run.agent_id),
  );
}

export function handleOf(team: Agent[], agentId: string): string {
  return team.find((agent) => agent.id === agentId)?.handle ?? agentId;
}
