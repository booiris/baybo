import type { AdminClient } from '../../api/client';
import type { components } from '../../api/schema';
import type { Agent, Issue, IssueStatus, Project } from './boardModel';

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

export async function fetchAgents(client: AdminClient): Promise<Outcome<Agent[]>> {
  try {
    const { data, error, response } = await client.GET('/v1/agents');
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined) return failure(response.status, error.error);
    if (!response.ok) return failure(response.status, undefined);
    return { kind: 'ok', value: data.items };
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
