import { useCallback, useEffect, useMemo, useState, type KeyboardEvent, type ReactNode } from 'react';
import {
  RiCloseLine,
  RiGitBranchLine,
  RiLoader4Line,
  RiRefreshLine,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';

type SessionSummary = components['schemas']['SessionSummary'];
type SessionDetail = components['schemas']['SessionDetail'];
type SessionParentLink = components['schemas']['SessionParentLink'];

const thCell =
  'px-4 py-3 text-left font-bold text-[0.78rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white whitespace-nowrap';
const tdCell = 'px-4 py-3 border-b border-gray-200 text-[0.86rem] align-top';

function shortId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 8)}...${id.slice(-4)}` : id;
}

function formatDate(iso: string | null | undefined): string {
  if (!iso) return '-';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString([], {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

function formatError(error: unknown, fallback: string): string {
  if (error !== null && error !== undefined) {
    try {
      return JSON.stringify(error);
    } catch {
      return JSON.stringify({ error: String(error) });
    }
  }
  return JSON.stringify({ error: fallback });
}

function parentLinkLabel(parentLink: SessionParentLink): string {
  return `${parentLink.kind} from ${shortId(parentLink.session_id)}`;
}

export function SessionsPage() {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [items, setItems] = useState<SessionSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const { data, error: apiError, response } = await client.GET('/v1/sessions');
      if (response.status === 401) {
        logout();
        return;
      }
      if (apiError || !response.ok) {
        setError(formatError(apiError, `HTTP Error ${response.status}`));
        setItems([]);
        return;
      }
      setItems(data?.items ?? []);
    } catch (e) {
      setError(formatError({ error: e instanceof Error ? e.message : String(e) }, 'Network error'));
      setItems([]);
    } finally {
      setLoading(false);
    }
  }, [client, logout]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const sortedItems = useMemo(
    () =>
      [...items].sort(
        (a, b) => new Date(b.last_active).getTime() - new Date(a.last_active).getTime(),
      ),
    [items],
  );

  const selectFromKeyboard = (event: KeyboardEvent<HTMLTableRowElement>, id: string) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      setSelectedId(id);
    }
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
            SESSIONS
          </h2>
          <span className="text-[0.9rem] text-ink-soft font-mono">
            {loading ? 'Loading...' : `${sortedItems.length.toLocaleString()} total`}
          </span>
        </div>
        <Button
          onClick={() => void reload()}
          disabled={loading}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
        >
          {loading ? (
            <RiLoader4Line className="text-base animate-spin" />
          ) : (
            <RiRefreshLine className="text-base" />
          )}
          Refresh
        </Button>
      </div>

      {error && <ErrorStrip message={error} />}

      <div className="flex-1 min-h-0 flex overflow-hidden bg-white border-[3px] border-black rounded-md shadow-brutal">
        <section className="flex-1 min-w-0 flex flex-col overflow-hidden border-r-2 border-black">
          <div className="flex-1 overflow-auto overscroll-none">
            <table className="w-full border-collapse">
              <thead>
                <tr>
                  <th className={thCell}>ID</th>
                  <th className={thCell}>User</th>
                  <th className={thCell}>Channel</th>
                  <th className={thCell}>Trigger</th>
                  <th className={thCell}>Parent Link</th>
                  <th className={thCell}>Last Active</th>
                  <th className={`${thCell} text-right`}>Messages</th>
                </tr>
              </thead>
              <tbody>
                {sortedItems.length === 0 && !loading && (
                  <tr>
                    <td colSpan={7} className="px-6 py-10 text-center text-ink-soft text-[0.9rem]">
                      No sessions found.
                    </td>
                  </tr>
                )}
                {sortedItems.map((session) => {
                  const selected = selectedId === session.id;
                  return (
                    <tr
                      key={session.id}
                      tabIndex={0}
                      aria-current={selected ? 'true' : undefined}
                      onClick={() => setSelectedId(session.id)}
                      onKeyDown={(event) => selectFromKeyboard(event, session.id)}
                      className={`cursor-pointer outline-none hover:bg-gray-50 focus:bg-gray-50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand ${
                        selected ? 'bg-brand/10' : ''
                      }`}
                    >
                      <td className={`${tdCell} font-mono`} title={session.id}>
                        {shortId(session.id)}
                      </td>
                      <td className={tdCell}>
                        <code className="font-mono break-all">{session.user_id}</code>
                      </td>
                      <td className={tdCell}>{session.channel}</td>
                      <td className={tdCell}>{session.trigger}</td>
                      <td className={tdCell}>
                        {session.parent_link ? (
                          <span className="font-mono text-[0.8rem]" title={session.parent_link.session_id}>
                            {parentLinkLabel(session.parent_link)}
                          </span>
                        ) : (
                          <span className="text-ink-soft">-</span>
                        )}
                      </td>
                      <td className={tdCell}>{formatDate(session.last_active)}</td>
                      <td className={`${tdCell} text-right font-mono`}>{session.message_count}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </section>

        <SessionDetailPanel
          sessionId={selectedId}
          onClose={() => setSelectedId(null)}
          onForked={(child) => {
            setSelectedId(child.id);
            void reload();
          }}
        />
      </div>
    </div>
  );
}

function SessionDetailPanel({
  sessionId,
  onClose,
  onForked,
}: {
  sessionId: string | null;
  onClose: () => void;
  onForked: (child: SessionDetail) => void;
}) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [forkJobId, setForkJobId] = useState('');
  const [forking, setForking] = useState(false);

  const reload = useCallback(async () => {
    if (!sessionId) {
      setDetail(null);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const { data, error: apiError, response } = await client.GET('/v1/sessions/{id}', {
        params: { path: { id: sessionId } },
      });
      if (response.status === 401) {
        logout();
        return;
      }
      if (apiError || !response.ok) {
        setError(formatError(apiError, `HTTP Error ${response.status}`));
        setDetail(null);
        return;
      }
      setDetail(data ?? null);
    } catch (e) {
      setError(formatError({ error: e instanceof Error ? e.message : String(e) }, 'Network error'));
      setDetail(null);
    } finally {
      setLoading(false);
    }
  }, [client, logout, sessionId]);

  useEffect(() => {
    setForkJobId('');
    void reload();
  }, [reload]);

  const fork = useCallback(async () => {
    if (!sessionId) return;
    const atJobId = forkJobId.trim();
    if (!atJobId) {
      setError(formatError({ error: 'at_job_id is required' }, 'Missing at_job_id'));
      return;
    }

    setForking(true);
    setError(null);
    try {
      const { data, error: apiError, response } = await client.POST('/v1/sessions/{id}/fork', {
        params: { path: { id: sessionId } },
        body: { at_job_id: atJobId },
      });
      if (response.status === 401) {
        logout();
        return;
      }
      if (apiError || !response.ok) {
        setError(formatError(apiError, `HTTP Error ${response.status}`));
        return;
      }
      if (data) {
        setForkJobId('');
        onForked(data);
      }
    } catch (e) {
      setError(formatError({ error: e instanceof Error ? e.message : String(e) }, 'Network error'));
    } finally {
      setForking(false);
    }
  }, [client, forkJobId, logout, onForked, sessionId]);

  return (
    <aside className="w-[430px] shrink-0 flex flex-col bg-white overflow-hidden">
      <header className="flex items-center gap-2 px-4 py-3 border-b-2 border-black">
        <h3 className="text-lg font-bold uppercase tracking-wider">Detail</h3>
        <div className="flex-1" />
        {sessionId && (
          <IconButton onClick={onClose} aria-label="Close session detail">
            <RiCloseLine className="text-xl" />
          </IconButton>
        )}
      </header>

      {!sessionId && (
        <div className="flex-1 flex items-center justify-center px-8 text-center text-ink-soft text-[0.9rem]">
          Select a session to inspect its metadata.
        </div>
      )}

      {sessionId && (
        <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
          {error && <ErrorStrip message={error} compact />}
          {loading && (
            <div className="px-4 py-6 text-center">
              <RiLoader4Line className="text-2xl text-ink-soft animate-spin inline-block" />
            </div>
          )}
          {detail && (
            <div className="flex-1 overflow-auto px-4 py-4 flex flex-col gap-4">
              <Field label="ID">
                <code className="font-mono text-sm break-all">{detail.id}</code>
              </Field>
              <Field label="User">
                <code className="font-mono text-sm break-all">{detail.user_id}</code>
              </Field>
              <Field label="Channel">{detail.channel}</Field>
              <Field label="Trigger">{detail.trigger}</Field>
              <Field label="Parent link">
                {detail.parent_link ? (
                  <ParentLinkBlock parentLink={detail.parent_link} />
                ) : (
                  <span className="text-ink-soft">-</span>
                )}
              </Field>
              <Field label="Created">{formatDate(detail.created_at)}</Field>
              <Field label="Last active">{formatDate(detail.last_active)}</Field>
              <Field label="Message count">{detail.message_count}</Field>
              <Field label="Active skills">
                {detail.active_skills.length === 0 ? (
                  <span className="text-ink-soft">-</span>
                ) : (
                  <ul className="list-disc list-inside text-sm">
                    {detail.active_skills.map((skill) => (
                      <li key={skill} className="break-all">
                        {skill}
                      </li>
                    ))}
                  </ul>
                )}
              </Field>
              <Field label="Compression count">{detail.compression_count}</Field>

              <form
                className="border-t-2 border-black pt-4 mt-2 flex flex-col gap-3"
                onSubmit={(event) => {
                  event.preventDefault();
                  void fork();
                }}
              >
                <h4 className="text-sm font-bold uppercase tracking-wider flex items-center gap-2">
                  <RiGitBranchLine />
                  Fork
                </h4>
                <label className="flex flex-col gap-1">
                  <span className="text-xs font-bold uppercase tracking-wider text-ink-soft">
                    at_job_id
                  </span>
                  <input
                    value={forkJobId}
                    onChange={(event) => setForkJobId(event.target.value)}
                    placeholder="job id"
                    className="px-3 py-2 border-2 border-black rounded-md font-mono text-sm bg-white"
                  />
                </label>
                <Button type="submit" disabled={forking} className="justify-center">
                  {forking ? (
                    <>
                      <RiLoader4Line className="text-base animate-spin" />
                      Forking
                    </>
                  ) : (
                    'Fork Session'
                  )}
                </Button>
              </form>
            </div>
          )}
        </div>
      )}
    </aside>
  );
}

function ParentLinkBlock({ parentLink }: { parentLink: SessionParentLink }) {
  return (
    <dl className="grid grid-cols-[80px_1fr] gap-x-3 gap-y-1 font-mono text-sm">
      <dt className="font-bold text-ink-soft">kind</dt>
      <dd>{parentLink.kind}</dd>
      <dt className="font-bold text-ink-soft">session</dt>
      <dd className="break-all">{parentLink.session_id}</dd>
      <dt className="font-bold text-ink-soft">at_job_id</dt>
      <dd className="break-all">{parentLink.at_job_id}</dd>
      <dt className="font-bold text-ink-soft">at_span_id</dt>
      <dd className="break-all">{parentLink.at_span_id ?? '-'}</dd>
    </dl>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-1">
      <span className="text-xs font-bold uppercase tracking-wider text-ink-soft">{label}</span>
      <div className="text-[0.95rem]">{children}</div>
    </section>
  );
}

function ErrorStrip({ message, compact = false }: { message: string; compact?: boolean }) {
  return (
    <div
      className={`bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words ${
        compact ? 'm-4 mb-0' : 'mb-4'
      }`}
      role="alert"
    >
      {message}
    </div>
  );
}
