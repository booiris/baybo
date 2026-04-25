import { useEffect, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { RiLoader4Line } from 'react-icons/ri';
import { Button } from '../components/Button';
import { SelectBox } from '../components/SelectBox';
import { useMockMode, MOCK_TRACES, type TraceSession } from '../api/mock';

const PAGE_SIZE_OPTIONS = [20, 50, 100];
const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  const date = d.toLocaleDateString('sv-SE');
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
  return { date, time };
}

export function TracesPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();

  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(20);
  const [loading] = useState(false);
  const [items, setItems] = useState<TraceSession[]>([]);
  const [total, setTotal] = useState(0);
  const [selected, setSelected] = useState<TraceSession | null>(null);

  useEffect(() => {
    if (isMock) {
      setTotal(MOCK_TRACES.length);
      setItems(MOCK_TRACES.slice(offset, offset + pageSize));
    } else {
      setItems([]);
      setTotal(0);
    }
  }, [isMock, offset, pageSize]);

  useEffect(() => {
    setOffset(0);
  }, [pageSize]);

  const toggleMock = () => {
    const newParams = new URLSearchParams(searchParams);
    if (isMock) {
      newParams.delete('mock');
    } else {
      newParams.set('mock', 'true');
    }
    setSearchParams(newParams);
    setOffset(0);
  };

  const pageStart = items.length === 0 ? 0 : offset + 1;
  const pageEnd = offset + items.length;
  const hasPrev = offset > 0;
  const hasNext = pageEnd < total;

  const statusBadge = (status: TraceSession['status']) => {
    const map = {
      active: 'bg-info text-white',
      completed: 'bg-ok text-white',
      error: 'bg-err text-white',
    };
    return (
      <span
        className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${map[status]}`}
      >
        {status}
      </span>
    );
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
            TRACES
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
        </div>
      </div>

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-collapse table-fixed">
            <thead>
              <tr>
                <th className={`${thCell} w-[160px]`}>Create Time</th>
                <th className={`${thCell} w-[160px]`}>Active Time</th>
                <th className={`${thCell} w-[240px]`}>Session ID</th>
                <th className={`${thCell} w-[200px]`}>Trigger</th>
                <th className={`${thCell}`}>Last Message</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[80px]`}>Spans</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && !loading && (
                <tr>
                  <td
                    colSpan={7}
                    className="px-6 py-10 text-center text-ink-soft text-[0.9rem]"
                  >
                    No sessions found. {!isMock && 'Enable Mock mode to see generated sessions.'}
                  </td>
                </tr>
              )}
              {items.map((session, idx) => {
                const createTs = splitTimestamp(session.createTime);
                const activeTs = splitTimestamp(session.activeTime);
                const notLast = idx !== items.length - 1;
                const cell = `px-6 py-4 align-top ${notLast ? 'border-b border-black' : ''}`;
                return (
                  <tr 
                    key={session.id} 
                    className="hover:bg-gray-50 cursor-pointer group"
                    onClick={() => setSelected(session)}
                  >
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug whitespace-nowrap">
                        {createTs.date}
                        <br />
                        {createTs.time}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug whitespace-nowrap">
                        {activeTs.date}
                        <br />
                        {activeTs.time}
                      </div>
                    </td>
                    <td className={cell}>
                      <code className="font-mono text-[0.9rem] break-all group-hover:text-brand">{session.id}</code>
                    </td>
                    <td className={cell}>
                      <span className="text-[0.9rem] break-words font-bold">{session.trigger}</span>
                    </td>
                    <td className={cell}>
                      <div className="text-[0.9rem] text-ink-soft truncate" title={session.lastMessage}>
                        {session.lastMessage}
                      </div>
                    </td>
                    <td className={cell}>{statusBadge(session.status)}</td>
                    <td className={cell}>
                      <span className="text-[0.9rem]">{session.spanCount}</span>
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
                <RiLoader4Line className="animate-spin" /> Loading sessions...
              </span>
            ) : total === 0 ? (
              'No sessions'
            ) : (
              `Showing ${pageStart} to ${pageEnd} of ${total.toLocaleString()} sessions`
            )}
          </span>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <span className="text-[0.85rem] text-ink-soft whitespace-nowrap">Per page:</span>
              <SelectBox
                value={pageSize}
                onChange={(e) => setPageSize(Number(e.target.value))}
                className="!py-1 !px-2 !pr-8 text-[0.85rem] h-8"
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

      {selected && <TraceDetailModal session={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function TraceDetailModal({ session, onClose }: { session: TraceSession; onClose: () => void }) {
  const createTs = splitTimestamp(session.createTime);
  const activeTs = splitTimestamp(session.activeTime);
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <div
        className="max-w-2xl w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="flex items-center justify-between px-6 py-4 border-b-2 border-black">
          <div className="flex items-center gap-3">
            <code className="font-mono text-[0.9rem] font-bold">{session.id}</code>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
          >
            Close
          </button>
        </header>
        <div className="px-6 py-4 space-y-4">
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 font-mono text-[0.85rem]">
            <div className="contents">
              <dt className="font-bold text-ink-soft">Trigger</dt>
              <dd className="break-words font-bold">{session.trigger}</dd>
            </div>
            <div className="contents">
              <dt className="font-bold text-ink-soft">Last Message</dt>
              <dd className="break-words italic">"{session.lastMessage}"</dd>
            </div>
            <div className="contents">
              <dt className="font-bold text-ink-soft">Status</dt>
              <dd className="break-words uppercase">{session.status}</dd>
            </div>
            <div className="contents">
              <dt className="font-bold text-ink-soft">Create Time</dt>
              <dd className="break-words">{createTs.date} {createTs.time}</dd>
            </div>
            <div className="contents">
              <dt className="font-bold text-ink-soft">Active Time</dt>
              <dd className="break-words">{activeTs.date} {activeTs.time}</dd>
            </div>
            <div className="contents">
              <dt className="font-bold text-ink-soft">Span Count</dt>
              <dd className="break-words">{session.spanCount}</dd>
            </div>
          </dl>
          <div className="pt-4 border-t-2 border-black text-center text-ink-soft text-[0.85rem]">
            Detailed span view coming soon.
          </div>
        </div>
      </div>
    </div>
  );
}
