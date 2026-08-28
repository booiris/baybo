import { useEffect, useState, type ChangeEvent } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  RiAlarmLine,
  RiRefreshLine,
  RiEyeLine,
  RiTimeLine,
  RiChatSmile2Line,
  RiLoader4Line,
  RiDeleteBinLine,
  RiPauseLine,
  RiPlayLine,
  RiArrowGoBackLine,
  RiPencilLine,
  RiSaveLine,
  RiErrorWarningLine,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SelectBox } from '../components/SelectBox';
import { SearchBox } from '../components/SearchBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { AdminClient } from '../api/client';
import type { components, operations } from '../api/schema';
import { useMockMode, MOCK_CRONS } from '../api/mock';

export type McpToolGrant = components['schemas']['McpToolGrant'];
export type GrantableMcpTool =
  operations['list_grantable_mcp_tools']['responses'][200]['content']['application/json']['items'][number];
export type CronJob = components['schemas']['CronJob'];
type CronStatus = components['schemas']['CronStatus'];
type CronSchedule = components['schemas']['CronSchedule'];
export type UpdateCronRequest = components['schemas']['UpdateCronRequest'];

// Default page size for cron jobs
const DEFAULT_PAGE_SIZE = 20;
const PAGE_SIZE_OPTIONS = [10, 20, 50, 100];

const MOCK_BLOCKED = 'Cron mutations are disabled in mock mode.';

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

const fieldLabel = 'block text-[0.7rem] font-bold uppercase text-ink-soft mb-1';

const textInput =
  'w-full px-3 py-2 bg-white border-2 border-black rounded-md shadow-brutal-xs font-mono text-[0.9rem] outline-none disabled:opacity-60 disabled:bg-canvas disabled:cursor-not-allowed';

const modalNotice =
  'flex items-start gap-2 bg-gray-50 border-2 border-black rounded-md shadow-brutal-xs px-3 py-2 text-[0.8rem] leading-snug';

const STATUS_BADGE_STYLE: Record<CronStatus, string> = {
  enabled: 'bg-ok text-white',
  disabled: 'bg-gray-200 text-ink-soft',
  executed: 'bg-brand text-ink',
};

/** Which list the page is showing: the live jobs, or the recycle bin. */
export type CronView = 'live' | 'deleted';

/** The bodyless `POST /v1/cron/{id}/<action>` routes. */
export type CronAction = 'pause' | 'resume' | 'restore';

export type CronListOutcome =
  | { kind: 'ok'; items: CronJob[] }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

export type McpToolListOutcome =
  | { kind: 'ok'; items: GrantableMcpTool[] }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

export type CronMutationOutcome =
  | { kind: 'ok' }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

/** An edit answers with the job it produced, so nothing has to be guessed here. */
export type CronUpdateOutcome =
  | { kind: 'ok'; job: CronJob }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

/** The dialog painted over the page, if any. */
export type CronModal = 'detail' | 'edit' | 'trash' | null;

function networkMessage(e: unknown): string {
  return e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway';
}

function responseErrorMessage(
  error: components['schemas']['ErrorBody'] | undefined,
  status: number,
): string {
  const message = error?.error;
  return message !== null && message !== undefined && message.length > 0
    ? message
    : `HTTP Error ${status}`;
}

function compareMcpGrants(a: McpToolGrant, b: McpToolGrant): number {
  if (a.tool_name !== b.tool_name) return a.tool_name < b.tool_name ? -1 : 1;
  if (a.transport_identity === b.transport_identity) return 0;
  return a.transport_identity < b.transport_identity ? -1 : 1;
}

export function mcpGrantKey(grant: McpToolGrant): string {
  return JSON.stringify([grant.tool_name, grant.transport_identity]);
}

export function normalizeMcpGrants(grants: readonly McpToolGrant[]): McpToolGrant[] {
  const byExactTuple = new Map<string, McpToolGrant>();
  for (const grant of grants) {
    byExactTuple.set(mcpGrantKey(grant), { ...grant });
  }
  return [...byExactTuple.values()].sort(compareMcpGrants);
}

export function mcpGrantForTool(tool: GrantableMcpTool): McpToolGrant {
  return {
    tool_name: tool.tool,
    transport_identity: tool.transport_identity,
  };
}

function includesExactGrant(grants: readonly McpToolGrant[], wanted: McpToolGrant): boolean {
  const wantedKey = mcpGrantKey(wanted);
  return grants.some((grant) => mcpGrantKey(grant) === wantedKey);
}

export function replaceMcpGrantSelection(
  selected: readonly McpToolGrant[],
  grant: McpToolGrant,
  checked: boolean,
): McpToolGrant[] {
  const withoutExactTuple = selected.filter((item) => mcpGrantKey(item) !== mcpGrantKey(grant));
  return normalizeMcpGrants(checked ? [...withoutExactTuple, grant] : withoutExactTuple);
}

export function mcpGrantReplacement(
  persisted: readonly McpToolGrant[],
  selected: readonly McpToolGrant[],
): McpToolGrant[] | undefined {
  const before = normalizeMcpGrants(persisted);
  const after = normalizeMcpGrants(selected);
  if (
    before.length === after.length
    && before.every((grant, index) => mcpGrantKey(grant) === mcpGrantKey(after[index]))
  ) {
    return undefined;
  }
  return after;
}

export interface McpGrantOption {
  kind: 'live' | 'stale';
  server: string;
  upstream: string;
  description: string;
  grant: McpToolGrant;
  selected: boolean;
  staleReason?: string;
}

export interface McpGrantGroup {
  server: string;
  options: McpGrantOption[];
}

export function groupMcpGrantOptions(
  available: readonly GrantableMcpTool[],
  selected: readonly McpToolGrant[],
): McpGrantGroup[] {
  const normalizedSelection = normalizeMcpGrants(selected);
  const liveByTuple = new Map<string, GrantableMcpTool>();
  const liveByName = new Map<string, GrantableMcpTool>();
  const options: McpGrantOption[] = [];

  for (const tool of available) {
    const grant = mcpGrantForTool(tool);
    const key = mcpGrantKey(grant);
    if (liveByTuple.has(key)) continue;
    liveByTuple.set(key, tool);
    liveByName.set(tool.tool, tool);
    options.push({
      kind: 'live',
      server: tool.server,
      upstream: tool.upstream,
      description: tool.description,
      grant,
      selected: includesExactGrant(normalizedSelection, grant),
    });
  }

  for (const grant of normalizedSelection) {
    if (liveByTuple.has(mcpGrantKey(grant))) continue;
    const sameName = liveByName.get(grant.tool_name);
    options.push({
      kind: 'stale',
      server: sameName?.server ?? 'Unavailable grants',
      upstream: sameName?.upstream ?? grant.tool_name,
      description: 'Persisted grant; it does not match a currently connected operation.',
      grant,
      selected: true,
      staleReason: sameName
        ? 'The live operation has a different transport identity.'
        : 'The server is disconnected, or this operation was removed.',
    });
  }

  const grouped = new Map<string, McpGrantOption[]>();
  for (const option of options) {
    const group = grouped.get(option.server) ?? [];
    group.push(option);
    grouped.set(option.server, group);
  }

  return [...grouped.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([server, groupOptions]) => ({
      server,
      options: groupOptions.sort((a, b) => {
        if (a.kind !== b.kind) return a.kind === 'stale' ? -1 : 1;
        return compareMcpGrants(a.grant, b.grant);
      }),
    }));
}

export function retainedStaleMcpGrants(
  available: readonly GrantableMcpTool[],
  selected: readonly McpToolGrant[],
): McpToolGrant[] {
  const liveKeys = new Set(available.map((tool) => mcpGrantKey(mcpGrantForTool(tool))));
  return normalizeMcpGrants(selected).filter((grant) => !liveKeys.has(mcpGrantKey(grant)));
}

export type McpServerBulkIntent =
  | { kind: 'noop' }
  | {
      kind: 'confirm';
      server: string;
      additionCount: number;
      warning: string;
      selection: McpToolGrant[];
    };

export function planMcpServerBulkSelect(
  group: McpGrantGroup,
  selected: readonly McpToolGrant[],
): McpServerBulkIntent {
  const additions = group.options
    .filter((option) => option.kind === 'live' && !includesExactGrant(selected, option.grant))
    .map((option) => option.grant);
  if (additions.length === 0) return { kind: 'noop' };
  return {
    kind: 'confirm',
    server: group.server,
    additionCount: additions.length,
    warning: `Bulk grant warning: select all ${additions.length} remaining operation${additions.length === 1 ? '' : 's'} from server "${group.server}"? Each selected operation will be available to every future fire of this cron job.`,
    selection: normalizeMcpGrants([...selected, ...additions]),
  };
}

/**
 * Which slot paints a mutation failure. A modal's overlay covers the page-level
 * banner, so a failure raised while a modal is open has to render *inside* that
 * modal — painted on the page it would sit under the scrim, unreadable.
 */
export function mutationErrorSlot(
  message: string | null,
  openModal: CronModal,
): 'none' | 'page' | Exclude<CronModal, null> {
  if (message === null || message.length === 0) return 'none';
  return openModal ?? 'page';
}

/**
 * A paused job resumes, a running one pauses; an executed one-shot has nothing
 * left to schedule, so it offers neither.
 */
export function toggleActionFor(status: CronStatus): 'pause' | 'resume' | null {
  if (status === 'enabled') return 'pause';
  if (status === 'disabled') return 'resume';
  return null;
}

export async function fetchCronJobs(client: AdminClient, view: CronView): Promise<CronListOutcome> {
  try {
    const { data, error, response } = await client.GET('/v1/cron', {
      params: { query: { deleted: view === 'deleted' } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined || !response.ok) {
      return { kind: 'failed', message: responseErrorMessage(error, response.status) };
    }
    return { kind: 'ok', items: data?.items ?? [] };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

interface ForwardMcpToolsClient {
  GET(path: '/v1/cron/mcp-tools'): Promise<{
    data?: { items: GrantableMcpTool[]; next_cursor?: string | null };
    error?: components['schemas']['ErrorBody'];
    response: Response;
  }>;
}

export async function fetchCronMcpTools(client: AdminClient): Promise<McpToolListOutcome> {
  try {
    // The route and DTOs are present in the backend change while generated TS
    // is intentionally updated later; this structural seam remains compatible
    // with the generated client once that update lands.
    const forwardClient = client as unknown as ForwardMcpToolsClient;
    const { data, error, response } = await forwardClient.GET('/v1/cron/mcp-tools');
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined || !response.ok) {
      return { kind: 'failed', message: responseErrorMessage(error, response.status) };
    }
    return { kind: 'ok', items: data?.items ?? [] };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

async function runMutation(
  call: () => Promise<{ error?: components['schemas']['ErrorBody']; response: Response }>,
): Promise<CronMutationOutcome> {
  try {
    const { error, response } = await call();
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined || !response.ok) {
      return { kind: 'failed', message: responseErrorMessage(error, response.status) };
    }
    return { kind: 'ok' };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/** Soft delete: the row moves to the recycle bin, it does not disappear. */
export function deleteCronJob(client: AdminClient, id: string): Promise<CronMutationOutcome> {
  return runMutation(() => client.DELETE('/v1/cron/{id}', { params: { path: { id } } }));
}

export function actOnCronJob(
  client: AdminClient,
  id: string,
  action: CronAction,
): Promise<CronMutationOutcome> {
  switch (action) {
    case 'pause':
      return runMutation(() => client.POST('/v1/cron/{id}/pause', { params: { path: { id } } }));
    case 'resume':
      // 400 when a one-shot's moment has already passed — the caller surfaces it.
      return runMutation(() => client.POST('/v1/cron/{id}/resume', { params: { path: { id } } }));
    case 'restore':
      return runMutation(() => client.POST('/v1/cron/{id}/restore', { params: { path: { id } } }));
  }
}

/**
 * Edit a job in place, keeping its id and its history. The body is a patch: a
 * field it leaves out is left untouched. The gateway answers with the edited
 * job — `next_trigger_at` included, recomputed from now when the schedule or
 * the timezone moved. It refuses a patch that sets nothing and an `at` that has
 * already passed (400), and a job in the recycle bin (404); the caller surfaces
 * all three.
 */
export async function updateCronJob(
  client: AdminClient,
  id: string,
  patch: UpdateCronRequest,
): Promise<CronUpdateOutcome> {
  try {
    const { data, error, response } = await client.PATCH('/v1/cron/{id}', {
      params: { path: { id } },
      body: patch,
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error !== undefined || !response.ok) {
      return { kind: 'failed', message: responseErrorMessage(error, response.status) };
    }
    return { kind: 'ok', job: data };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/** The edit modal's boxes. The patch is the diff between this and the job. */
export interface CronEditForm {
  title: string;
  prompt: string;
  timezone: string;
  scheduleKind: CronSchedule['kind'];
  /** 5-field cron expression; carries the schedule when the kind is `cron`. */
  expr: string;
  /** A `datetime-local` value in the viewer's own zone; used when the kind is `at`. */
  at: string;
}

function pad(n: number): string {
  return String(n).padStart(2, '0');
}

/** An instant → the value a `datetime-local` box shows, in the viewer's own zone. */
export function isoToLocalInput(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** That box's value → the instant the gateway stores. Null when it is not a time. */
function isoFromLocalInput(value: string): string | null {
  const ms = Date.parse(value);
  return Number.isNaN(ms) ? null : new Date(ms).toISOString();
}

export function jobToEditForm(job: CronJob): CronEditForm {
  return {
    title: job.title,
    prompt: job.prompt,
    timezone: job.timezone,
    scheduleKind: job.schedule.kind,
    expr: job.schedule.kind === 'cron' ? job.schedule.expr : '',
    at: job.schedule.kind === 'at' ? isoToLocalInput(job.schedule.time) : '',
  };
}

/** A box left blank that the gateway would only refuse. Save stays disabled. */
export function cronEditIncomplete(form: CronEditForm): boolean {
  if (!form.timezone.trim() || !form.prompt.trim()) return true;
  return form.scheduleKind === 'cron'
    ? !form.expr.trim()
    : isoFromLocalInput(form.at) === null;
}

function editedSchedule(initial: CronEditForm, form: CronEditForm): CronSchedule | null {
  if (form.scheduleKind === 'cron') {
    const expr = form.expr.trim();
    if (!expr || (initial.scheduleKind === 'cron' && expr === initial.expr.trim())) return null;
    return { kind: 'cron', expr };
  }
  const time = isoFromLocalInput(form.at);
  // Compared as instants, not as text: the box holds only minutes, so a job whose
  // `at` carries seconds must still read as untouched when nobody touched it.
  if (time === null || (initial.scheduleKind === 'at' && time === isoFromLocalInput(initial.at))) {
    return null;
  }
  return { kind: 'at', time };
}

/**
 * The PATCH body — only the boxes the user actually moved. Leaving an untouched
 * field out is what keeps the edit from re-arming a schedule nobody changed:
 * re-sending the same expression would still recompute the next fire from now.
 */
export function cronEditPatch(
  job: CronJob,
  form: CronEditForm,
  selectedMcpToolGrants?: readonly McpToolGrant[],
): UpdateCronRequest {
  const initial = jobToEditForm(job);
  const patch: UpdateCronRequest = {};
  if (form.title !== initial.title) patch.title = form.title;
  if (form.prompt !== initial.prompt) patch.prompt = form.prompt;
  if (form.timezone.trim() !== initial.timezone) patch.timezone = form.timezone.trim();
  const schedule = editedSchedule(initial, form);
  if (schedule) patch.schedule = schedule;
  if (selectedMcpToolGrants !== undefined) {
    const replacement = mcpGrantReplacement(job.mcp_tool_grants ?? [], selectedMcpToolGrants);
    if (replacement !== undefined) patch.mcp_tool_grants = replacement;
  }
  return patch;
}

function formatTimestamp(iso: string | null | undefined): string {
  if (iso === null || iso === undefined || iso.length === 0) return '-';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString('sv-SE', {
    dateStyle: 'short',
    timeStyle: 'short',
  });
}

export function CronPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [view, setView] = useState<CronView>('live');
  const [filter, setFilter] = useState('');
  const [debouncedFilter, setDebouncedFilter] = useState('');
  const [statusFilter, setStatusFilter] = useState<'all' | CronStatus>('all');
  const [channelFilter, setChannelFilter] = useState<'all' | string>('all');
  const [scheduleKindFilter, setScheduleKindFilter] = useState<'all' | 'cron' | 'at'>('all');

  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [allItems, setAllItems] = useState<CronJob[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CronJob | null>(null);
  const [editing, setEditing] = useState<CronJob | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [pendingDeleteId, setPendingDeleteId] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);
  const [mutationError, setMutationError] = useState<string | null>(null);

  const deletedView = view === 'deleted';

  // Topmost first — that is the dialog a failure has to render inside.
  const openModal: CronModal = pendingDeleteId !== null
    ? 'trash'
    : editing !== null
      ? 'edit'
      : selected !== null
        ? 'detail'
        : null;
  const errorSlot = mutationErrorSlot(mutationError, openModal);

  // Debounce the filter input
  useEffect(() => {
    const handle = window.setTimeout(() => setDebouncedFilter(filter), 250);
    return () => window.clearTimeout(handle);
  }, [filter]);

  // Reset offset on filter change
  useEffect(() => {
    setOffset(0);
  }, [debouncedFilter, statusFilter, channelFilter, scheduleKindFilter, pageSize, view]);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setAllItems(MOCK_CRONS.filter((job) => Boolean(job.deleted_at) === deletedView));
        setLoading(false);
        setError(null);
        return;
      }

      setLoading(true);
      setError(null);

      const outcome = await fetchCronJobs(client, view);
      if (canceled) return;
      setLoading(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setAllItems(outcome.items);
    }
    void fetchData();
    return () => { canceled = true; };
  }, [client, logout, refreshKey, isMock, view, deletedView]);

  // Client-side filtering
  const filteredItems = allItems.filter(item => {
    if (statusFilter !== 'all' && item.status !== statusFilter) return false;
    if (channelFilter !== 'all' && item.channel !== channelFilter) return false;
    if (scheduleKindFilter !== 'all' && item.schedule.kind !== scheduleKindFilter) return false;

    if (debouncedFilter.trim()) {
      const q = debouncedFilter.toLowerCase().trim();
      const matchId = item.id.toLowerCase().includes(q);
      const matchTitle = item.title.toLowerCase().includes(q);
      const matchPrompt = item.prompt.toLowerCase().includes(q);
      if (!matchId && !matchTitle && !matchPrompt) return false;
    }

    return true;
  });

  // Unique channels for filter dropdown
  const availableChannels = Array.from(new Set(allItems.map(i => i.channel))).sort();

  // Client-side pagination
  const items = filteredItems.slice(offset, offset + pageSize);
  const total = filteredItems.length;
  const pageStart = items.length === 0 ? 0 : offset + 1;
  const pageEnd = offset + items.length;
  const hasPrev = offset > 0;
  const hasNext = pageEnd < total;

  const mutate = async (id: string, run: () => Promise<CronMutationOutcome>): Promise<void> => {
    if (isMock) {
      setMutationError(MOCK_BLOCKED);
      return;
    }
    setMutating(true);
    setMutationError(null);
    const outcome = await run();
    setMutating(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setMutationError(outcome.message);
      return;
    }
    setPendingDeleteId(null);
    // The row's status / list membership just changed server-side: drop the
    // detail modal's stale copy and refetch rather than patching in place.
    setSelected((cur) => (cur?.id === id ? null : cur));
    setRefreshKey((k) => k + 1);
  };

  const handleDelete = (id: string): Promise<void> => mutate(id, () => deleteCronJob(client, id));

  const handleAction = (id: string, action: CronAction): Promise<void> =>
    mutate(id, () => actOnCronJob(client, id, action));

  const handleEditSave = async (id: string, patch: UpdateCronRequest): Promise<void> => {
    if (isMock) {
      setMutationError(MOCK_BLOCKED);
      return;
    }
    setMutating(true);
    setMutationError(null);
    const outcome = await updateCronJob(client, id, patch);
    setMutating(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      // Stays in the edit modal: the boxes keep what the user typed, and the
      // refusal renders next to them instead of under the modal's overlay.
      setMutationError(outcome.message);
      return;
    }
    setEditing(null);
    // The edit answered with the job it produced, so the detail view behind this
    // modal takes it as-is; the list still refetches for the row.
    setSelected((cur) => (cur?.id === outcome.job.id ? outcome.job : cur));
    setRefreshKey((k) => k + 1);
  };

  const openEditor = (job: CronJob) => {
    setMutationError(null);
    setEditing(job);
  };

  const toggleMock = () => {
    const newParams = new URLSearchParams(searchParams);
    if (isMock) {
      newParams.delete('mock');
    } else {
      newParams.set('mock', 'true');
    }
    setSearchParams(newParams);
  };

  const statusBadge = (status: CronStatus) => (
    <span
      className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${STATUS_BADGE_STYLE[status]}`}
    >
      <span>{status}</span>
    </span>
  );

  const scheduleDisplay = (
    schedule: components['schemas']['CronSchedule'],
    timezone: string,
  ) => {
    if (schedule.kind === 'cron') {
      return (
        <div className="flex items-center gap-2 font-mono text-[0.9rem]">
          <RiTimeLine className="text-ink-soft text-lg shrink-0" />
          <span>{schedule.expr}</span>
          <span className="text-[0.7rem] text-ink-soft">({timezone})</span>
        </div>
      );
    }
    return (
      <div className="flex items-center gap-2 font-mono text-[0.9rem]">
        <RiAlarmLine className="text-ink-soft text-lg shrink-0" />
        <span>{formatTimestamp(schedule.time)}</span>
      </div>
    );
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
            CRON JOBS
          </h2>
        </div>
        <div className="flex gap-3">
          {import.meta.env.DEV && (
            <Button
              variant={isMock ? 'primary' : 'default'}
              onClick={toggleMock}
              className="!py-2 !px-4 !text-[0.9rem] h-10 w-[140px] justify-center gap-1.5"
            >
              {isMock ? 'Mock: ON' : 'Mock: OFF'}
            </Button>
          )}
          <Button
            onClick={() => setRefreshKey((k) => k + 1)}
            disabled={loading || isMock}
            className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
          >
            <RiRefreshLine className="text-lg shrink-0" /> Refresh
          </Button>
        </div>
      </div>

      <div className="flex items-center gap-3 mb-4">
        <SelectBox
          aria-label="Job list view"
          value={view}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => {
            setMutationError(null);
            setView(e.target.value as CronView);
          }}
          className="h-10 px-3 min-w-[160px]"
        >
          <option value="live">Live Jobs</option>
          <option value="deleted">Recycle Bin</option>
        </SelectBox>

        <SearchBox
          placeholder="Filter by ID or message..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-10"
        />

        <SelectBox
          value={statusFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setStatusFilter(e.target.value as any)}
          className="h-10 px-3"
        >
          <option value="all">All Status</option>
          <option value="enabled">Enabled</option>
          <option value="disabled">Disabled</option>
          <option value="executed">Executed</option>
        </SelectBox>

        <SelectBox
          value={scheduleKindFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setScheduleKindFilter(e.target.value as any)}
          className="h-10 px-3"
        >
          <option value="all">All Types</option>
          <option value="cron">Recurring (Cron)</option>
          <option value="at">One-shot (At)</option>
        </SelectBox>

        <SelectBox
          value={channelFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setChannelFilter(e.target.value)}
          className="h-10 px-3 min-w-[140px]"
        >
          <option value="all">All Channels</option>
          {availableChannels.map(ch => (
            <option key={ch} value={ch}>{ch.toUpperCase()}</option>
          ))}
        </SelectBox>
      </div>

      {error !== null && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      )}

      {/* Row-level pause / resume have no modal of their own; the confirm, detail
          and edit modals each render their own copy of this while open, since a
          banner painted here would sit under their overlay. */}
      {errorSlot === 'page' && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {mutationError}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={`${thCell} w-[150px]`}>ID</th>
                <th className={`${thCell} w-[200px]`}>Title</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[280px]`}>Schedule</th>
                <th className={`${thCell} w-[120px]`}>Channel</th>
                <th className={thCell}>Action</th>
                <th className={`${thCell} w-[160px]`}>Created At</th>
                <th className={`${thCell} w-[160px]`}>
                  {deletedView ? 'Deleted At' : 'Next Trigger'}
                </th>
                <th className={`${thCell} w-[150px] text-right`}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && !loading && (
                <tr>
                  <td
                    colSpan={9}
                    className="px-6 py-10 text-center text-ink-soft text-[0.9rem]"
                  >
                    {deletedView ? 'Recycle bin is empty.' : 'No cron jobs found.'}
                  </td>
                </tr>
              )}
              {items.map((job) => {
                const cell = `px-6 py-4 align-middle border-b border-black`;
                const toggle = toggleActionFor(job.status);
                return (
                  <tr key={job.id} className="hover:bg-gray-50">
                    <td className={cell}>
                      <code className="font-mono text-[0.85rem]">{job.id}</code>
                    </td>
                    <td className={cell}>
                      {/* Jobs created before titles existed fall back to their prompt. */}
                      <span className="text-[0.9rem] font-bold line-clamp-1">
                        {job.title || job.prompt}
                      </span>
                    </td>
                    <td className={cell}>{statusBadge(job.status)}</td>
                    <td className={cell}>{scheduleDisplay(job.schedule, job.timezone)}</td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-bold uppercase tracking-wider">{job.channel}</span>
                    </td>
                    <td className={cell}>
                      <div className="flex items-center gap-2">
                        <RiChatSmile2Line className="text-brand shrink-0 text-lg" />
                        <span className="text-[0.9rem] line-clamp-1">{job.prompt}</span>
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(job.created_at)}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(deletedView ? job.deleted_at : job.next_trigger_at)}
                      </div>
                    </td>
                    <td className={`${cell} text-right`}>
                      <div className="inline-flex gap-1">
                        <IconButton
                          aria-label="View job detail"
                          onClick={() => setSelected(job)}
                        >
                          <RiEyeLine />
                        </IconButton>
                        {deletedView ? (
                          <IconButton
                            aria-label="Restore cron job"
                            title="Restore from the recycle bin"
                            onClick={() => {
                              void handleAction(job.id, 'restore');
                            }}
                            disabled={isMock || mutating}
                            className="!border-ok !text-ok hover:!bg-ok/10"
                          >
                            <RiArrowGoBackLine />
                          </IconButton>
                        ) : (
                          <>
                            <IconButton
                              aria-label="Edit cron job"
                              title="Edit: prompt, title, schedule or timezone"
                              onClick={() => openEditor(job)}
                              disabled={isMock || mutating}
                            >
                              <RiPencilLine />
                            </IconButton>
                            {toggle && (
                              <IconButton
                                aria-label={toggle === 'pause' ? 'Pause cron job' : 'Resume cron job'}
                                title={
                                  toggle === 'pause'
                                    ? 'Pause: stop firing until resumed'
                                    : 'Resume: next trigger recomputed from now'
                                }
                                onClick={() => {
                                  void handleAction(job.id, toggle);
                                }}
                                disabled={isMock || mutating}
                              >
                                {toggle === 'pause' ? <RiPauseLine /> : <RiPlayLine />}
                              </IconButton>
                            )}
                            {/* A built-in job has a config switch and a
                                pause button; the server refuses to delete
                                it, so offering the affordance would only
                                produce a 400. */}
                            {job.builtin !== true && (
                              <IconButton
                                aria-label="Move cron job to recycle bin"
                                title="Move to the recycle bin"
                                onClick={() => {
                                  setMutationError(null);
                                  setPendingDeleteId(job.id);
                                }}
                                disabled={isMock || mutating}
                                className="!border-err !text-err hover:!bg-err/10"
                              >
                                <RiDeleteBinLine />
                              </IconButton>
                            )}
                          </>
                        )}
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        <div className="flex justify-between items-center px-4 py-3 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft min-w-[200px]">
            {loading ? (
              <span className="flex items-center gap-2">
                <RiLoader4Line className="animate-spin" /> Loading jobs...
              </span>
            ) : total === 0 ? (
              deletedView ? 'Recycle bin is empty' : 'No cron jobs'
            ) : (
              `Showing ${pageStart} to ${pageEnd} of ${total.toLocaleString()} jobs`
            )}
          </span>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <span className="text-[0.85rem] text-ink-soft whitespace-nowrap">Per page:</span>
              <SelectBox
                value={pageSize}
                onChange={(e: ChangeEvent<HTMLSelectElement>) => setPageSize(Number(e.target.value))}
                className="h-8 px-2"
              >
                {PAGE_SIZE_OPTIONS.map((opt) => (
                  <option key={opt} value={opt}>
                    {opt}
                  </option>
                ))}
              </SelectBox>
            </div>
            <div className="flex gap-2">
              <Button
                onClick={() => setOffset((o) => Math.max(0, o - pageSize))}
                disabled={!hasPrev || loading}
                className="!py-1 !px-3 !text-[0.85rem] h-8"
              >
                Prev
              </Button>
              <Button
                onClick={() => setOffset((o) => o + pageSize)}
                disabled={!hasNext || loading}
                className="!py-1 !px-3 !text-[0.85rem] h-8"
              >
                Next
              </Button>
            </div>
          </div>
        </div>
      </div>

      {selected && (
        <CronDetailModal
          job={selected}
          submitting={mutating}
          error={errorSlot === 'detail' ? mutationError : null}
          onClose={() => {
            setMutationError(null);
            setSelected(null);
          }}
          onEdit={selected.deleted_at !== null && selected.deleted_at !== undefined
            ? undefined
            : () => openEditor(selected)}
          onTrash={
            (selected.deleted_at !== null && selected.deleted_at !== undefined)
              || selected.builtin === true
              ? undefined
              : () => {
                  setMutationError(null);
                  setPendingDeleteId(selected.id);
                }
          }
          onRestore={
            selected.deleted_at !== null && selected.deleted_at !== undefined
              ? () => {
                  void handleAction(selected.id, 'restore');
                }
              : undefined
          }
        />
      )}
      {editing && (
        <CronEditModal
          key={editing.id}
          job={editing}
          client={client}
          isMock={isMock}
          submitting={mutating}
          error={errorSlot === 'edit' ? mutationError : null}
          onUnauthorized={logout}
          onClose={() => {
            setMutationError(null);
            setEditing(null);
          }}
          onSave={(patch) => {
            void handleEditSave(editing.id, patch);
          }}
        />
      )}
      {pendingDeleteId !== null && (
        <TrashConfirmModal
          id={pendingDeleteId}
          submitting={mutating}
          error={errorSlot === 'trash' ? mutationError : null}
          onCancel={() => {
            setPendingDeleteId(null);
            setMutationError(null);
          }}
          onConfirm={() => {
            void handleDelete(pendingDeleteId);
          }}
        />
      )}
    </div>
  );
}

function CronDetailModal({
  job,
  submitting,
  error,
  onClose,
  onEdit,
  onTrash,
  onRestore,
}: {
  job: CronJob;
  submitting: boolean;
  error: string | null;
  onClose: () => void;
  onEdit?: () => void;
  onTrash?: () => void;
  onRestore?: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <div
        className="max-w-2xl w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden max-h-full flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="shrink-0 flex items-center justify-between gap-6 px-6 py-4 border-b-2 border-black">
          <div className="flex items-center gap-3 min-w-0">
            <h3 className="font-bold uppercase tracking-wider shrink-0">Cron Job Detail</h3>
            <code className="font-mono text-[0.9rem] bg-gray-100 px-2 py-0.5 rounded border border-black truncate">{job.id}</code>
          </div>
          <div className="flex items-center gap-4 shrink-0">
            {onEdit && (
              <button
                type="button"
                onClick={onEdit}
                disabled={submitting}
                className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer inline-flex items-center gap-1"
              >
                <RiPencilLine className="text-base" /> Edit
              </button>
            )}
            {onRestore && (
              <button
                type="button"
                onClick={onRestore}
                disabled={submitting}
                className="text-[0.85rem] font-bold uppercase tracking-wider text-ok hover:text-ok/80 cursor-pointer inline-flex items-center gap-1"
              >
                <RiArrowGoBackLine className="text-base" /> Restore
              </button>
            )}
            {onTrash && (
              <button
                type="button"
                onClick={onTrash}
                className="text-[0.85rem] font-bold uppercase tracking-wider text-err hover:text-err/80 cursor-pointer inline-flex items-center gap-1"
              >
                <RiDeleteBinLine className="text-base" /> Move to Bin
              </button>
            )}
            <button
              type="button"
              onClick={onClose}
              className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
            >
              Close
            </button>
          </div>
        </header>
        <div className="px-6 py-4 space-y-4 overflow-y-auto min-h-0">
          {error !== null && (
            <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
              {error}
            </div>
          )}
          <div className="grid grid-cols-2 gap-4">
            <div>
              <label className={fieldLabel}>User ID</label>
              <div className="font-mono text-[0.9rem] break-all">{job.user_id}</div>
            </div>
            <div>
              <label className={fieldLabel}>Channel</label>
              <div className="font-bold uppercase text-[0.9rem]">{job.channel}</div>
            </div>
            <div>
              <label className={fieldLabel}>Created At</label>
              <div className="text-[0.9rem]">{formatTimestamp(job.created_at)}</div>
            </div>
            <div>
              <label className={fieldLabel}>Last Triggered</label>
              <div className="text-[0.9rem]">{formatTimestamp(job.last_triggered_at)}</div>
            </div>
            <div>
              <label className={fieldLabel}>Timezone</label>
              <div className="font-mono text-[0.9rem]">{job.timezone}</div>
            </div>
            {job.deleted_at !== null && job.deleted_at !== undefined && (
              <div>
                <label className={fieldLabel}>Deleted At</label>
                <div className="text-[0.9rem]">{formatTimestamp(job.deleted_at)}</div>
              </div>
            )}
          </div>

          <div>
            <label className={fieldLabel}>Prompt</label>
            <div className="bg-gray-50 border-2 border-black rounded-md px-4 py-3 font-mono text-[0.9rem] break-words">
              {job.prompt}
            </div>
          </div>

          {job.origin_session_id !== null && job.origin_session_id !== undefined && (
            <div>
              <label className={fieldLabel}>Origin Session</label>
              <code className="font-mono text-[0.85rem] break-all">{job.origin_session_id}</code>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function CronEditModal({
  job,
  client,
  isMock,
  submitting,
  error,
  onUnauthorized,
  onClose,
  onSave,
}: {
  job: CronJob;
  client: AdminClient;
  isMock: boolean;
  submitting: boolean;
  error: string | null;
  onUnauthorized: () => void;
  onClose: () => void;
  onSave: (patch: UpdateCronRequest) => void;
}) {
  const [form, setForm] = useState<CronEditForm>(() => jobToEditForm(job));
  const [selectedMcpToolGrants, setSelectedMcpToolGrants] = useState<McpToolGrant[]>(() =>
    normalizeMcpGrants(job.mcp_tool_grants ?? []),
  );
  const [grantableMcpTools, setGrantableMcpTools] = useState<GrantableMcpTool[]>([]);
  const [mcpToolsLoadState, setMcpToolsLoadState] = useState<
    'loading' | 'ok' | 'failed' | 'mock'
  >(isMock ? 'mock' : 'loading');
  const [mcpToolsError, setMcpToolsError] = useState<string | null>(null);
  const [mcpToolsRefreshKey, setMcpToolsRefreshKey] = useState(0);

  useEffect(() => {
    if (isMock) {
      setMcpToolsLoadState('mock');
      setMcpToolsError(null);
      setGrantableMcpTools([]);
      return;
    }
    let canceled = false;
    setMcpToolsLoadState('loading');
    setMcpToolsError(null);
    async function loadMcpTools() {
      const outcome = await fetchCronMcpTools(client);
      if (canceled) return;
      if (outcome.kind === 'unauthorized') {
        onUnauthorized();
        return;
      }
      if (outcome.kind === 'failed') {
        setGrantableMcpTools([]);
        setMcpToolsError(outcome.message);
        setMcpToolsLoadState('failed');
        return;
      }
      setGrantableMcpTools(outcome.items);
      setMcpToolsLoadState('ok');
    }
    void loadMcpTools();
    return () => {
      canceled = true;
    };
  }, [client, isMock, mcpToolsRefreshKey, onUnauthorized]);

  const patch = cronEditPatch(job, form, selectedMcpToolGrants);
  const changed = Object.keys(patch);
  const mcpGroups = groupMcpGrantOptions(grantableMcpTools, selectedMcpToolGrants);
  const staleMcpGrants = mcpToolsLoadState === 'ok'
    ? retainedStaleMcpGrants(grantableMcpTools, selectedMcpToolGrants)
    : [];
  const selectedMcpGrantsUnverified =
    mcpToolsLoadState !== 'ok'
    && mcpToolsLoadState !== 'mock'
    && selectedMcpToolGrants.length > 0;
  const blocked =
    submitting
    || changed.length === 0
    || cronEditIncomplete(form)
    || staleMcpGrants.length > 0
    || selectedMcpGrantsUnverified;
  const reschedules = patch.schedule !== undefined || patch.timezone !== undefined;

  const set = <K extends keyof CronEditForm>(key: K, value: CronEditForm[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  const selectServerOperations = (group: McpGrantGroup) => {
    const intent = planMcpServerBulkSelect(group, selectedMcpToolGrants);
    if (intent.kind === 'confirm' && window.confirm(intent.warning)) {
      setSelectedMcpToolGrants(intent.selection);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <div
        className="max-w-2xl w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden max-h-full flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="shrink-0 flex items-center justify-between gap-6 px-6 py-4 border-b-2 border-black">
          <div className="flex items-center gap-3 min-w-0">
            <h3 className="font-bold uppercase tracking-wider shrink-0">Edit Cron Job</h3>
            <code className="font-mono text-[0.9rem] bg-gray-100 px-2 py-0.5 rounded border border-black truncate">
              {job.id}
            </code>
          </div>
          <button
            type="button"
            onClick={onClose}
            disabled={submitting}
            className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer shrink-0"
          >
            Close
          </button>
        </header>

        <div className="px-6 py-4 space-y-4 overflow-y-auto min-h-0">
          {error !== null && (
            <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
              {error}
            </div>
          )}

          {job.status === 'disabled' && (
            <div className={modalNotice}>
              <RiPauseLine className="text-base shrink-0 mt-0.5 text-ink-soft" />
              <span>
                This job is <strong className="font-bold uppercase">paused</strong>, and saving keeps
                it paused — an edit never restarts a job. Resume it when you want it running again.
              </span>
            </div>
          )}
          {job.status === 'executed' && (
            <div className={modalNotice}>
              <RiAlarmLine className="text-base shrink-0 mt-0.5 text-ink-soft" />
              <span>
                This one-shot has already fired. Give it a one-shot time in the future to arm it
                again — it keeps its id and its past runs.
              </span>
            </div>
          )}

          {job.builtin === true && (
            <div className="rounded-md border border-line bg-surface-2 px-3 py-2 text-[0.8rem] text-ink-soft">
              This job is part of Baybo. Its schedule is yours to change; its name
              and instruction are the runtime's, and the server refuses edits to
              them. Switch it off for good in <code>baybo.json</code>.
            </div>
          )}

          <div className="grid grid-cols-2 gap-4">
            <div className="col-span-2">
              <label className={fieldLabel} htmlFor="cron-edit-title">
                Title
              </label>
              <input
                id="cron-edit-title"
                className={textInput}
                value={form.title}
                disabled={submitting || job.builtin === true}
                onChange={(e) => set('title', e.target.value)}
                placeholder="e.g. Morning digest"
              />
            </div>

            <div>
              <label className={fieldLabel} htmlFor="cron-edit-kind">
                Schedule Type
              </label>
              <SelectBox
                id="cron-edit-kind"
                className="w-full h-10 !border-2 !text-[0.9rem]"
                value={form.scheduleKind}
                disabled={submitting}
                onChange={(e: ChangeEvent<HTMLSelectElement>) =>
                  set('scheduleKind', e.target.value as CronSchedule['kind'])
                }
              >
                <option value="cron">Recurring (Cron)</option>
                <option value="at">One-shot (At)</option>
              </SelectBox>
            </div>

            <div>
              <label className={fieldLabel} htmlFor="cron-edit-timezone">
                Timezone
              </label>
              <input
                id="cron-edit-timezone"
                className={`${textInput} h-10`}
                value={form.timezone}
                disabled={submitting}
                onChange={(e) => set('timezone', e.target.value)}
                placeholder="e.g. Asia/Shanghai"
              />
            </div>

            <div className="col-span-2">
              {form.scheduleKind === 'cron' ? (
                <>
                  <label className={fieldLabel} htmlFor="cron-edit-expr">
                    Cron Expression
                  </label>
                  <input
                    id="cron-edit-expr"
                    className={textInput}
                    value={form.expr}
                    disabled={submitting}
                    onChange={(e) => set('expr', e.target.value)}
                    placeholder="0 9 * * *"
                  />
                  <p className="mt-1 text-[0.75rem] text-ink-soft">
                    Five fields, evaluated in the job's timezone.
                  </p>
                </>
              ) : (
                <>
                  <label className={fieldLabel} htmlFor="cron-edit-at">
                    Fires At
                  </label>
                  <input
                    id="cron-edit-at"
                    type="datetime-local"
                    className={textInput}
                    value={form.at}
                    disabled={submitting}
                    onChange={(e) => set('at', e.target.value)}
                  />
                  <p className="mt-1 text-[0.75rem] text-ink-soft">
                    A single moment, in your local time. A time that has already passed is refused.
                  </p>
                </>
              )}
            </div>
          </div>

          <div>
            <label className={fieldLabel} htmlFor="cron-edit-prompt">
              Prompt
            </label>
            <textarea
              id="cron-edit-prompt"
              className={`${textInput} resize-y`}
              rows={4}
              value={form.prompt}
              disabled={submitting || job.builtin === true}
              onChange={(e) => set('prompt', e.target.value)}
              placeholder="What the job should do when it fires"
            />
          </div>

          <section className="border-2 border-black rounded-md overflow-hidden shadow-brutal-xs">
            <div className="flex items-start justify-between gap-4 px-3 py-3 bg-canvas border-b-2 border-black">
              <div>
                <h4 className="font-bold uppercase tracking-wider text-[0.8rem]">
                  MCP Operations
                </h4>
                <p className="mt-1 text-[0.75rem] text-ink-soft leading-snug">
                  No operation is granted by default. Each checkbox is one exact operation and
                  transport identity; checking a current operation never upgrades an older grant.
                </p>
              </div>
              {mcpToolsLoadState === 'loading' && (
                <RiLoader4Line className="animate-spin text-lg shrink-0" aria-label="Loading MCP operations" />
              )}
            </div>

            {mcpToolsLoadState === 'mock' && (
              <div className="px-3 py-3 text-[0.8rem] text-ink-soft">
                Connected MCP operations are not loaded in mock mode.
              </div>
            )}

            {mcpToolsLoadState === 'failed' && (
              <div className="px-3 py-3 space-y-3">
                <div className="flex items-start gap-2 text-[0.8rem] text-err leading-snug">
                  <RiErrorWarningLine className="text-lg shrink-0" />
                  <span>
                    Could not verify connected MCP operations: {mcpToolsError}. Existing selections
                    are unchanged. Saving is blocked while any selected grant cannot be verified;
                    retry, or clear every saved grant to revoke them.
                  </span>
                </div>
                <Button
                  type="button"
                  onClick={() => setMcpToolsRefreshKey((key) => key + 1)}
                  disabled={submitting}
                  className="!py-1 !px-3 !text-[0.8rem] h-8 gap-1.5"
                >
                  <RiRefreshLine /> Retry MCP List
                </Button>
                {selectedMcpToolGrants.length > 0 && (
                  <Button
                    type="button"
                    onClick={() => setSelectedMcpToolGrants([])}
                    disabled={submitting}
                    className="!py-1 !px-3 !text-[0.8rem] h-8 ml-2 !border-err !text-err"
                  >
                    Revoke All Saved Grants
                  </Button>
                )}
              </div>
            )}

            {mcpToolsLoadState === 'loading' && (
              <div className="px-3 py-3 text-[0.8rem] text-ink-soft">
                Checking the currently connected operations and exact transport identities…
              </div>
            )}

            {mcpToolsLoadState === 'ok' && staleMcpGrants.length > 0 && (
              <div className="flex items-start gap-2 px-3 py-3 bg-brand/15 border-b-2 border-black text-[0.8rem] leading-snug">
                <RiErrorWarningLine className="text-lg shrink-0" />
                <span>
                  <strong>Save blocked:</strong> {staleMcpGrants.length} stale grant
                  {staleMcpGrants.length === 1 ? '' : 's'} remain selected. Clear each stale
                  checkbox to revoke it. If a replacement is live, select its separate unchecked
                  row explicitly.
                </span>
              </div>
            )}

            {mcpToolsLoadState === 'ok' && mcpGroups.length === 0 && (
              <div className="px-3 py-3 text-[0.8rem] text-ink-soft">
                No typed MCP operations are currently connected.
              </div>
            )}

            {mcpToolsLoadState === 'ok' && mcpGroups.map((group) => {
              const bulkIntent = planMcpServerBulkSelect(group, selectedMcpToolGrants);
              return (
                <fieldset key={group.server} className="border-b last:border-b-0 border-black">
                  <legend className="sr-only">MCP server {group.server}</legend>
                  <div className="flex items-center justify-between gap-3 px-3 py-2 bg-gray-50 border-b border-black">
                    <div className="min-w-0">
                      <span className="font-bold font-mono text-[0.85rem] break-all">
                        {group.server}
                      </span>
                      <span className="ml-2 text-[0.7rem] uppercase text-ink-soft">
                        {group.options.filter((option) => option.kind === 'live').length} live
                      </span>
                    </div>
                    <Button
                      type="button"
                      onClick={() => selectServerOperations(group)}
                      disabled={submitting || bulkIntent.kind === 'noop'}
                      title="Shows a warning before selecting every live operation on this server"
                      className="!py-1 !px-2 !text-[0.72rem] h-7 shrink-0"
                    >
                      Select all…
                    </Button>
                  </div>
                  <div className="divide-y divide-black">
                    {group.options.map((option) => (
                      <label
                        key={`${option.kind}:${mcpGrantKey(option.grant)}`}
                        className={`flex items-start gap-3 px-3 py-3 cursor-pointer ${
                          option.kind === 'stale' ? 'bg-brand/10' : 'bg-white'
                        }`}
                      >
                        <input
                          type="checkbox"
                          checked={option.selected}
                          disabled={submitting}
                          onChange={(event) =>
                            setSelectedMcpToolGrants((selected) =>
                              replaceMcpGrantSelection(
                                selected,
                                option.grant,
                                event.target.checked,
                              ),
                            )
                          }
                          className="mt-1 size-4 accent-black shrink-0"
                        />
                        <span className="min-w-0 flex-1">
                          <span className="flex flex-wrap items-center gap-2">
                            <strong className="font-mono text-[0.85rem] break-all">
                              {option.upstream}
                            </strong>
                            {option.kind === 'stale' && (
                              <span className="px-1.5 py-0.5 bg-brand border border-black rounded text-[0.65rem] font-bold uppercase">
                                Stale saved grant
                              </span>
                            )}
                          </span>
                          <code className="block mt-1 text-[0.72rem] break-all text-ink-soft">
                            tool: {option.grant.tool_name}
                          </code>
                          <code className="block mt-0.5 text-[0.68rem] break-all text-ink-soft">
                            transport: {option.grant.transport_identity}
                          </code>
                          <span className="block mt-1 text-[0.75rem] leading-snug">
                            {option.description}
                          </span>
                          {option.staleReason !== undefined && (
                            <span className="block mt-1 text-[0.75rem] font-bold text-err leading-snug">
                              {option.staleReason} Clear this checkbox to revoke the saved tuple.
                            </span>
                          )}
                        </span>
                      </label>
                    ))}
                  </div>
                </fieldset>
              );
            })}
          </section>

          {reschedules && job.status !== 'disabled' && (
            <div className={modalNotice}>
              <RiTimeLine className="text-base shrink-0 mt-0.5 text-ink-soft" />
              <span>
                The next fire time is recomputed from now — the runs this job missed are never made
                up.
              </span>
            </div>
          )}
        </div>

        <footer className="shrink-0 flex items-center justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
          <span className="mr-auto text-[0.75rem] text-ink-soft font-mono">
            {changed.length === 0 ? 'No changes yet' : `Sends only: ${changed.join(', ')}`}
          </span>
          <Button
            type="button"
            onClick={onClose}
            disabled={submitting}
            className="!py-1 !px-3 !text-[0.85rem] h-9"
          >
            Cancel
          </Button>
          <Button
            type="button"
            variant="primary"
            onClick={() => onSave(patch)}
            disabled={blocked}
            className="!py-1 !px-3 !text-[0.85rem] h-9 gap-1.5"
          >
            {submitting ? (
              <RiLoader4Line className="animate-spin text-base shrink-0" />
            ) : (
              <RiSaveLine className="text-base shrink-0" />
            )}
            Save Changes
          </Button>
        </footer>
      </div>
    </div>
  );
}

function TrashConfirmModal({
  id,
  submitting,
  error,
  onCancel,
  onConfirm,
}: {
  id: string;
  submitting: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onCancel}
    >
      <div
        className="max-w-md w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden max-h-full flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="shrink-0 px-6 py-4 border-b-2 border-black">
          <h3 className="font-bold uppercase tracking-wider">Move to Recycle Bin</h3>
        </header>
        <div className="px-6 py-4 space-y-3 overflow-y-auto min-h-0">
          <p className="text-[0.95rem]">
            Move cron job{' '}
            <code className="font-mono text-[0.85rem] bg-gray-100 px-1 rounded border border-black">
              {id}
            </code>
            {' '}to the recycle bin? It stops firing and leaves this list, but the record is kept —
            you can restore it from the Recycle Bin view.
          </p>
          {error !== null && (
            <div className="bg-white border-[2px] border-err text-err rounded-md px-3 py-2 font-mono text-[0.85rem] break-words">
              {error}
            </div>
          )}
        </div>
        <footer className="shrink-0 flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
          <Button
            type="button"
            onClick={onCancel}
            disabled={submitting}
            className="!py-1 !px-3 !text-[0.85rem] h-9"
          >
            Cancel
          </Button>
          <Button
            type="button"
            onClick={onConfirm}
            disabled={submitting}
            className="!py-1 !px-3 !text-[0.85rem] h-9 gap-1.5 !bg-err !text-white !border-err hover:!bg-err/90"
          >
            {submitting && <RiLoader4Line className="animate-spin text-base shrink-0" />}
            <RiDeleteBinLine className="text-base shrink-0" /> Move to Bin
          </Button>
        </footer>
      </div>
    </div>
  );
}
