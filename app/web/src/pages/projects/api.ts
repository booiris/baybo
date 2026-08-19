import type { AdminClient } from '../../api/client';
import type { components } from '../../api/schema';
import type { Agent, Issue, IssueRun, IssueStatus, Project, RunLog } from './boardModel';
import type { FeedEntry, IssueEvent } from './timelineModel';

export type CreateIssueRequest = components['schemas']['CreateIssueRequest'];
export type UpdateIssueRequest = components['schemas']['UpdateIssueRequest'];
export type IssueAttachmentRequest = components['schemas']['IssueAttachmentRequest'];

export type Outcome<T> =
  | { kind: 'ok'; value: T }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

function networkMessage(e: unknown): string {
  return e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway';
}

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
  // Framework and LLM pin are both on `HireAgentRequest` already; the
  // client narrowed them away, so the user form could not offer the two
  // knobs the spec deliberately gives it over the lead's own hiring tool.
  body: { name: string; role: string; framework?: Agent['framework']; llm?: string },
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
  body: {
    name: string;
    description: string;
    workdir?: string;
    daily_budget_micros?: number;
    daily_budget_tokens?: number;
    max_parallel_issue_runs?: number;
  },
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

/// One page of the conversation a run worked in.
///
/// A SESSION, not a run: one agent's runs on a card share it (a retry
/// continues where the last attempt left off), so an attempt's page also
/// holds the ones before it. The board slices it back apart by run.
export type RunTranscript = components['schemas']['ChatSessionDetail'];

/// Read a run's conversation.
///
/// A board route rather than the chat one it shares a body with, for two
/// reasons the panel depends on: it is addressed the way the execution log
/// addresses a run (card + attempt, never a session id the client had to
/// learn), and it is the ONE reader that is shown the run's brief — on a chat
/// surface that row is the framing the agent wrote itself, and here it is the
/// ask the rest of the page answers.
///
/// `beforeOrdinal` pages backwards; the newest page (`null`) is also the only
/// one the server folds a still-running turn's live steps into, which is why
/// following a run means re-reading THAT page rather than syncing forward.
export async function fetchRunTranscript(
  client: AdminClient,
  projectId: string,
  number: number,
  attempt: number,
  beforeOrdinal: number | null,
  limit: number,
): Promise<Outcome<RunTranscript>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/issues/{number}/runs/{attempt}/transcript',
      {
        params: {
          path: { project_id: projectId, number, attempt },
          query: { before_ordinal: beforeOrdinal ?? undefined, limit },
        },
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

export async function fetchIssueRuns(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<RunLog>> {
  try {
    const { data, error, response } = await client.GET(
      '/v1/projects/{project_id}/issues/{number}/runs',
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
  attachments: IssueAttachmentRequest[] = [],
): Promise<Outcome<IssueEvent>> {
  try {
    const { data, error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/comments',
      { params: { path: { project_id: projectId, number } }, body: { text, attachments } },
    );
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

export async function fetchFeed(
  client: AdminClient,
  projectId: string,
  beforeMs: number | null,
): Promise<Outcome<FeedEntry[]>> {
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

export async function updateProject(
  client: AdminClient,
  projectId: string,
  body: {
    name: string;
    description: string;
    daily_budget_micros?: number | null;
    daily_budget_tokens?: number | null;
    max_parallel_issue_runs: number;
  },
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

/// Note that the operator has opened this card. Per card, never per board:
/// reading the question asked on #3 is not reading the one asked on #7.
export async function markIssueRead(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/read',
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

/// Note that the operator has read the whole board, in one press. Every
/// card's own cursor moves — the same stamp [`markIssueRead`] writes, on all
/// of them at once — so this clears counts and nothing else.
export async function markProjectRead(
  client: AdminClient,
  projectId: string,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.POST('/v1/projects/{project_id}/read', {
      params: { path: { project_id: projectId } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: null };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/// Run this card's failed run again. The one action that discharges the
/// board's `failed` count without destroying or hiding the card.
export async function retryRun(
  client: AdminClient,
  projectId: string,
  number: number,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.POST(
      '/v1/projects/{project_id}/issues/{number}/runs/retry',
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

/// Pin (or unpin) an agent's model. Same door the Agents page uses — the
/// profile is a second view of one roster, not a second way to edit it.
export async function setAgentModel(
  client: AdminClient,
  agentId: string,
  llm: string,
): Promise<Outcome<null>> {
  try {
    const { error, response } = await client.PUT('/v1/agents/{agent_id}/model', {
      params: { path: { agent_id: agentId } },
      body: { llm },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: null };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/// The models this deployment can actually run, and which one `default-llm`
/// points at. A pin outside the pool is a teammate that fails every time it
/// is woken, so the picker offers the pool and nothing else.
export async function fetchModelPool(
  client: AdminClient,
): Promise<Outcome<{ names: string[]; defaultName: string }>> {
  try {
    const { data, error, response } = await client.GET('/v1/llm/models');
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return {
      kind: 'ok',
      value: {
        names: data.items.map((model) => model.name),
        defaultName: data.default_name,
      },
    };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}
