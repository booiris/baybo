import { useEffect, useState } from 'react';
import { RiDownloadLine, RiEyeLine, RiRefreshLine } from 'react-icons/ri';
import { Badge } from '../components/Badge';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SearchBox } from '../components/SearchBox';
import { SelectBox } from '../components/SelectBox';
import { fetchLogs } from '../mocks/logs';
import type { LogPage as LogPageData } from '../types';

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  return {
    date: d.toISOString().slice(0, 10),
    time: d.toISOString().slice(11, 23),
  };
}

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black';

export function LogPage() {
  const [data, setData] = useState<LogPageData | null>(null);
  const [loading, setLoading] = useState(false);

  const load = () => {
    setLoading(true);
    fetchLogs()
      .then(setData)
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
  }, []);

  return (
    <div className="p-8">
      <div className="flex justify-between items-start mb-8">
        <div>
          <h2 className="text-[2.5rem] font-bold uppercase -tracking-[0.05em] mb-2">
            SYSTEM LOGS
          </h2>
          <p className="text-ink-soft">Real-time event and error tracking.</p>
        </div>
        <Button variant="primary">
          <RiDownloadLine /> Export
        </Button>
      </div>

      <div className="flex gap-4 mb-6 bg-white border-[3px] border-black p-4 rounded-md shadow-brutal">
        <SearchBox placeholder="Filter by message or source..." />
        <SelectBox defaultValue="all">
          <option value="all">All Levels</option>
          <option value="error">Error</option>
          <option value="warn">Warn</option>
          <option value="info">Info</option>
        </SelectBox>
        <SelectBox defaultValue="24h">
          <option value="24h">Last 24 Hours</option>
          <option value="7d">Last 7 Days</option>
          <option value="all">All Time</option>
        </SelectBox>
        <Button onClick={load} disabled={loading}>
          <RiRefreshLine /> Refresh
        </Button>
      </div>

      <div className="bg-white border-[3px] border-black rounded-md shadow-brutal flex flex-col overflow-hidden">
        <table className="w-full border-collapse">
          <thead>
            <tr>
              <th className={thCell}>Timestamp</th>
              <th className={thCell}>Level</th>
              <th className={thCell}>Source</th>
              <th className={thCell}>Message</th>
              <th className={`${thCell} text-right`}>Action</th>
            </tr>
          </thead>
          <tbody>
            {data?.entries.map((log, idx) => {
              const { date, time } = splitTimestamp(log.timestamp);
              const notLast = idx !== data.entries.length - 1;
              const cell = `px-6 py-4 ${notLast ? 'border-b border-black' : ''}`;
              return (
                <tr key={log.id} className="hover:bg-gray-50">
                  <td className={cell}>
                    <div className="text-ink-soft text-[0.85rem] leading-snug">
                      {date}
                      <br />
                      {time}
                    </div>
                  </td>
                  <td className={cell}>
                    <Badge level={log.level} />
                  </td>
                  <td className={cell}>
                    <code className="font-mono bg-gray-100 px-2 py-1 rounded text-[0.9rem]">
                      {log.source}
                    </code>
                  </td>
                  <td className={cell}>{log.message}</td>
                  <td className={`${cell} text-right`}>
                    <IconButton aria-label="View details">
                      <RiEyeLine />
                    </IconButton>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>

        <div className="flex justify-between items-center px-6 py-4 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft">
            {data
              ? `Showing 1 to ${data.entries.length} of ${data.total.toLocaleString()} logs`
              : 'Loading...'}
          </span>
          <div className="flex gap-3">
            <Button disabled>Prev</Button>
            <Button>Next</Button>
          </div>
        </div>
      </div>
    </div>
  );
}
