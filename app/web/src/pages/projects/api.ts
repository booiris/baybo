import type { AdminClient } from '../../api/client';
import type { components, paths } from '../../api/schema';
import type { Agent, Issue, IssueRun, IssueStatus, Project } from './boardModel';
import type { IssueEvent } from './timelineModel';

export type CreateIssueRequest = components['schemas']['CreateIssueRequest'];
export type UpdateIssueRequest = components['schemas']['UpdateIssueRequest'];

/**
 * Every call answers with one of three shapes. `unauthorized` is separate
 * because it is the one outcome the page must not render — a dead token
 * has to log out, not paint an empty board.
 */
export type Outcome<T> =
  | { kind: 'ok'; value: T }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

function networkMessage(e: unknown): string {
  return e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway';
}

/** The gateway's error body, when it sent one; otherwise the bare status. */
function failure(status: number, detail: string | undefined): Outcome<never> {
  return {
    kind: 'failed',
    message: detail !== undefined && detail.length > 0 ? detail : `HTTP Error ${status}`,
  };
}

export async function fetchProjects(
  client: AdminClient,
  includeArchived: boolean,
): Promise<Outcome<Project[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects', {
      params: { query: { include_archived: includeArchived } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/**
 * This board's team — not `/v1/agents`, which lists global chat personas.
 * The two rosters are disjoint on the server: only a teammate can be
 * assigned an issue here, and only a global persona can be a chat persona.
 */
export async function fetchTeam(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<Agent[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/{project_id}/agents', {
      params: { path: { project_id: projectId } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function hireAgent(
  client: AdminClient,
  projectId: string,
  body: { name: string; role: string },
): Promise<Outcome<Agent>> {
  try {
    const { data, error, response } = await client.POST('/v1/projects/{project_id}/agents', {
      params: { path: { project_id: projectId } },
      body,
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function removeAgent(
  client: AdminClient,
  projectId: string,
  agentId: string,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.DELETE(
      '/v1/projects/{project_id}/agents/{agent_id}',
      { params: { path: { project_id: projectId, agent_id: agentId } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: null };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchProject(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<Project>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/{project_id}', {
      params: { path: { project_id: projectId } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function createProject(
  client: AdminClient,
  body: { name: string; description: string; workdir?: string },
): Promise<Outcome<Project>> {
  try {
    const { data, error, response } = await client.POST('/v1/projects', { body });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchIssues(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<Issue[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/{project_id}/issues', {
      params: { path: { project_id: projectId } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchActiveRuns(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<IssueRun[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/{project_id}/runs', {
      params: { path: { project_id: projectId } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchIssueRuns(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<IssueRun[]>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/issues/{number}/runs',
      { params: { path: { project_id: projectId, number } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchIssue(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<Issue>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/issues/{number}',
      { params: { path: { project_id: projectId, number } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function createIssue(
  client: AdminClient,
  projectId: string,
  body: CreateIssueRequest,
): Promise<Outcome<Issue>> {
  try {
    const { data, error, response } = await client.POST('/v1/projects/{project_id}/issues', {
      params: { path: { project_id: projectId } },
      body,
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function patchIssue(
  client: AdminClient,
  projectId: string,
  number: number,
  body: UpdateIssueRequest,
): Promise<Outcome<Issue>> {
  try {
    const { data, error, response } = await client.PATCH(
      '/v1/projects/{project_id}/issues/{number}',
      { params: { path: { project_id: projectId, number } }, body },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function moveIssue(
  client: AdminClient,
  projectId: string,
  number: number,
  status: IssueStatus,
  orderedNumbers: number[],
): Promise<Outcome<Issue>> {
  try {
    const { data, error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/move',
      {
        params: { path: { project_id: projectId, number } },
        body: { status, ordered_numbers: orderedNumbers },
      },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function cancelRun(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/runs/cancel',
      { params: { path: { project_id: projectId, number } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: null };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function retryRun(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<IssueRun>> {
  try {
    const { data, error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/runs/retry',
      { params: { path: { project_id: projectId, number } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchTimeline(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<IssueEvent[]>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/issues/{number}/events',
      { params: { path: { project_id: projectId, number } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function postComment(
  client: AdminClient,
  projectId: string,
  number: number,
  text: string,
): Promise<Outcome<IssueEvent>> {
  try {
    const { data, error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/comments',
      { params: { path: { project_id: projectId, number } }, body: { text } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/**
 * The whole board's activity, newest first — the same rows the per-issue
 * timelines are, read across the board instead of down one card.
 */
export async function fetchFeed(
  client: AdminClient,
  projectId: string,
  beforeMs: number | null,
): Promise<Outcome<IssueEvent[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/{project_id}/feed', {
      params: {
        path: { project_id: projectId },
        query: beforeMs == null ? {} : { before_ms: beforeMs },
      },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/**
 * Answer an approval a run is blocked on, from its card.
 *
 * Scoped to the issue on purpose: the approval queue is keyed by call id
 * alone, so the server checks the card exists on this board before it
 * answers anything.
 */
export async function resolveApproval(
  client: AdminClient,
  projectId: string,
  number: number,
  callId: string,
  decision: 'approve' | 'approve_always' | 'deny',
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/approvals/{call_id}',
      {
        params: { path: { project_id: projectId, number, call_id: callId } },
        body: { decision },
      },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: null };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export type LeadConversation =
  paths['/v1/projects/{project_id}/lead/conversations']['get']['responses'][200]['content']['application/json']['items'][number];

export async function fetchLeadConversations(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<LeadConversation[]>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/lead/conversations',
      { params: { path: { project_id: projectId } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function openLeadConversation(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<LeadConversation>> {
  try {
    const { data, error, response } = await client.POST(
      '/v1/projects/{project_id}/lead/conversations',
      { params: { path: { project_id: projectId } } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/** One transcript row the panel renders. */
export type LeadTurn = { id: string; kind: string; role: string; text: string; at: string };

/**
 * One conversation's newest page.
 *
 * The ordinary chat sync endpoint: it scopes by channel, not by whether a
 * session is on the chat *list*, so a board's thread reads through it
 * unchanged. Nothing here re-implements the transcript — `rows` is already
 * the flattened shape the chat client renders.
 */
export async function fetchLeadMessages(
  client: AdminClient,
  sessionId: string,
): Promise<Outcome<LeadTurn[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/chat/sessions/{session_id}/sync', {
      params: { path: { session_id: sessionId }, query: {} },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return {
      kind: 'ok',
      value: data.rows.map((row) => ({
        id: row.id,
        kind: row.kind,
        role: row.role,
        text: row.text,
        at: row.created_at,
      })),
    };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function updateProject(
  client: AdminClient,
  projectId: string,
  body: { name: string; description: string; daily_budget_micros?: number | null },
): Promise<Outcome<Project>> {
  try {
    const { data, error, response } = await client.PUT('/v1/projects/{project_id}', {
      params: { path: { project_id: projectId } },
      body,
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function setProjectArchived(
  client: AdminClient,
  projectId: string,
  archived: boolean,
): Promise<Outcome<Project>> {
  try {
    const { data, error, response } = await client.POST('/v1/projects/{project_id}/archive', {
      params: { path: { project_id: projectId } },
      body: { archived },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}
