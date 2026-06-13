import type { BenchInfo } from '../generated/BenchInfo';
import type { BenchDetail } from '../generated/BenchDetail';
import type { RunDetail } from '../generated/RunDetail';
import type { SearchHit } from '../generated/SearchHit';
import type { BenchTrace } from './types';

async function getJSON<T>(url: string): Promise<T> {
  const res = await fetch(url);
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new Error(`${res.status} ${res.statusText}${body ? `: ${body}` : ''}`);
  }
  return (await res.json()) as T;
}

const enc = encodeURIComponent;

export const api = {
  benches: () => getJSON<BenchInfo[]>('/api/benches'),

  bench: (id: string) => getJSON<BenchDetail>(`/api/benches/${enc(id)}`),

  run: (id: string, runKey: string) =>
    getJSON<RunDetail>(`/api/benches/${enc(id)}/runs/${enc(runKey)}`),

  search: (q: string) => getJSON<SearchHit[]>(`/api/search?q=${enc(q)}`),

  trace: (id: string, tracePath: string, messagesPath?: string | null) => {
    const params = new URLSearchParams({ trace: tracePath });
    if (messagesPath) params.set('messages', messagesPath);
    return getJSON<BenchTrace>(`/api/benches/${enc(id)}/trace?${params.toString()}`);
  },

  /** URL for a raw side-artifact (diff / verifier output / cast). */
  fileUrl: (id: string, path: string) =>
    `/api/benches/${enc(id)}/file?path=${enc(path)}`,

  async file(id: string, path: string): Promise<string> {
    const res = await fetch(this.fileUrl(id, path));
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return res.text();
  },
};
