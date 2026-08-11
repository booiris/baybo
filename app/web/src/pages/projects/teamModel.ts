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

/// Longest handle the grammar accepts. Mirrors `MAX_AGENT_HANDLE_CHARS`.
const MAX_HANDLE_CHARS = 32;

/// What the server will most likely derive as this name's `@handle`.
///
/// A **preview**, not the answer: `AgentHandle::derive`
/// (`crates/model/src/agent_profile.rs`) is the authority, and on a
/// collision the server appends a number this cannot know about. Shown
/// anyway because the handle is permanent, and committing to one sight
/// unseen is worse than committing to one that might gain a `-2`.
export function previewHandle(name: string): string | null {
  let slug = '';
  for (const ch of name) {
    if (/[a-zA-Z0-9]/.test(ch)) {
      slug += ch.toLowerCase();
    } else if (slug !== '' && !slug.endsWith('-')) {
      slug += '-';
    }
  }
  slug = slug.replace(/^-+/, '').replace(/-+$/, '').slice(0, MAX_HANDLE_CHARS);
  slug = slug.replace(/-+$/, '');
  // The grammar refuses a leading digit and an empty slug.
  if (slug === '' || /^[0-9]/.test(slug)) return null;
  return slug;
}
