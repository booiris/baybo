import { createAdminClient, type AdminClient } from './client';
import type { Connection } from './connection';

// Thin helpers over the generated admin client (docs/mac-app.md §6). The
// webview reaches the in-process gateway with the admin bearer token handed
// over by `get_connection`.

export interface SessionSummary {
  session_id: string;
  created_at: string;
  last_active: string;
  hidden?: boolean;
  last_user_text?: string | null;
}

export interface SlashCommand {
  command: string;
  description: string;
}

export interface ToolManifest {
  name: string;
  description: string;
}

export type CronScheduleView =
  | { kind: 'cron'; expr: string }
  | { kind: 'at'; time: string };

export interface CronJobView {
  id: string;
  prompt: string;
  schedule: CronScheduleView;
  status: 'enabled' | 'disabled' | 'executed';
  timezone: string;
  next_trigger_at?: string | null;
  last_triggered_at?: string | null;
}

export function admin(conn: Connection): AdminClient {
  return createAdminClient({ baseUrl: conn.baseUrl, token: conn.adminToken });
}

export async function listSessions(client: AdminClient): Promise<SessionSummary[]> {
  const { data } = await client.GET('/v1/chat/sessions', {});
  return (data?.items ?? []) as SessionSummary[];
}

export async function createSession(
  client: AdminClient,
): Promise<{ sessionId: string; token: string }> {
  const { data, error } = await client.POST('/v1/chat/sessions', {});
  if (error || !data) throw new Error('create session failed');
  return { sessionId: data.session_id, token: data.channel_token };
}

export async function refreshToken(client: AdminClient, sessionId: string): Promise<string> {
  const { data, error } = await client.POST('/v1/chat/sessions/{session_id}/token', {
    params: { path: { session_id: sessionId } },
  });
  if (error || !data) throw new Error('refresh token failed');
  return data.channel_token;
}

export async function hideSession(client: AdminClient, sessionId: string): Promise<void> {
  await client.DELETE('/v1/chat/sessions/{session_id}', {
    params: { path: { session_id: sessionId } },
  });
}

export async function listModels(client: AdminClient): Promise<string[]> {
  const { data } = await client.GET('/v1/llm/models', {});
  const items = (data?.items ?? []) as Array<{ name: string }>;
  return items.map((m) => m.name);
}

export async function setSessionModel(
  client: AdminClient,
  sessionId: string,
  llm: string | null,
): Promise<void> {
  await client.PUT('/v1/chat/sessions/{session_id}/model', {
    params: { path: { session_id: sessionId } },
    body: { llm },
  });
}

export async function slashManifest(client: AdminClient): Promise<SlashCommand[]> {
  const { data } = await client.GET('/v1/chat/slash-manifest', {});
  return (data?.items ?? []) as SlashCommand[];
}

/** Registered skills (the 插件 / Plugins view). The endpoint returns bare
 *  skill names. */
export async function listSkills(client: AdminClient): Promise<string[]> {
  const { data } = await client.GET('/v1/skills', {});
  return (data?.items ?? []) as string[];
}

/** Registered tool manifests (the 插件 / Plugins view). */
export async function listTools(client: AdminClient): Promise<ToolManifest[]> {
  const { data } = await client.GET('/v1/tools', {});
  const items = (data?.items ?? []) as Array<{ name: string; description: string }>;
  return items.map((t) => ({ name: t.name, description: t.description }));
}

/** Scheduled cron jobs (the 自动化 / Automation view). */
export async function listCron(client: AdminClient): Promise<CronJobView[]> {
  const { data } = await client.GET('/v1/cron', {});
  return (data?.items ?? []) as CronJobView[];
}

/** Upload bytes to the content-addressed blob store using the session's
 *  channel token (docs/mac-app.md §8). Returns the blob id to reference in a
 *  `WireAttachment`. */
export async function uploadBlob(
  conn: Connection,
  channelToken: string,
  file: File,
): Promise<string> {
  const buf = await file.arrayBuffer();
  const res = await fetch(`${conn.baseUrl}/v1/blobs`, {
    method: 'POST',
    headers: {
      'x-aura-channel-token': channelToken,
      'Content-Type': file.type || 'application/octet-stream',
    },
    body: buf,
  });
  if (!res.ok) throw new Error(`upload failed: HTTP ${res.status}`);
  const data = (await res.json()) as { blob_id: string };
  return data.blob_id;
}
