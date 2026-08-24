import { describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { IssueDetailPage } from './IssueDetailPage';
import type { Issue } from './boardModel';

const PROJECT_ID = '01JPROJECT';
const ok = { status: 200, ok: true } as Response;

type Attachment = { blob_id: string; mime_type: string; size: number; filename?: string };

const row: Issue & { attachments: Attachment[] } = {
  number: 7,
  project_id: PROJECT_ID,
  title: 'card 7',
  description: '',
  attachments: [],
  status: 'backlog',
  priority: 'none',
  position: 0,
  stage: 0,
  created_at_ms: 0,
  updated_at_ms: 0,
} as unknown as Issue & { attachments: Attachment[] };

const patched: unknown[] = [];

const client = {
  GET: vi.fn(async (path: string) => {
    if (path === '/v1/projects/{project_id}/issues/{number}') {
      return { data: { ...row }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues') {
      return { data: { items: [{ ...row }] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/runs') {
      return { data: { items: [] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/events') {
      return { data: { items: [] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/agents') {
      return { data: { items: [] }, error: undefined, response: ok };
    }
    throw new Error(`unexpected GET ${path}`);
  }),
  POST: vi.fn(),
  PATCH: vi.fn(async (_path: string, init: { body: { attachments?: Attachment[] } }) => {
    patched.push(JSON.parse(JSON.stringify(init.body)) as unknown);
    if (init.body.attachments !== undefined) {
      row.attachments = init.body.attachments.map((a) => ({
        blob_id: a.blob_id,
        mime_type: 'image/png',
        size: 10,
        ...(a.filename === undefined ? {} : { filename: a.filename }),
      }));
    }
    return { data: { ...row }, error: undefined, response: ok };
  }),
};

vi.mock('./MarkdownEditor', () => ({
  MarkdownEditor: ({ ariaLabel }: { ariaLabel: string }) => <textarea aria-label={ariaLabel} />,
}));

const auth = { logout: vi.fn(), baseUrl: 'http://board.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

let uploads = 0;
vi.stubGlobal(
  'fetch',
  vi.fn(async (_url: string, init?: { method?: string }) => {
    if (init?.method === 'POST') {
      uploads += 1;
      return {
        ok: true,
        status: 200,
        json: async () => ({ blob_id: `sha256:UP${String(uploads)}.t` }),
      } as unknown as Response;
    }
    // GET of blob bytes for the thumbnail: never resolves.
    return new Promise<Response>(() => undefined);
  }),
);

/// The other half of the draft-versus-row contract: a change the operator
/// makes must reach the card exactly once, and must not lose the file's name
/// on the way. The save is driven off blob ids, so the NAME is precisely what
/// a request rebuilt from those ids would silently drop.
describe('attaching and removing a file on an open card', () => {
  it('saves each change once, keeping the file’s name', async () => {
    const { container } = render(
      <MemoryRouter initialEntries={[`/projects/${PROJECT_ID}/issues/7`]}>
        <Routes>
          <Route path="/projects/:pid/issues/:num" element={<IssueDetailPage />} />
        </Routes>
      </MemoryRouter>,
    );
    await screen.findAllByText('card 7');
    const input = container.querySelector('input[type=file]');
    if (input === null) throw new Error('no file input');
    const file = new File(['xx'], 'shot.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [file], configurable: true });
    input.dispatchEvent(new Event('change', { bubbles: true }));
    await waitFor(() => {
      expect(patched.length).toBeGreaterThan(0);
    });
    // Long enough that a second, redundant save would have landed.
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(patched).toEqual([
      { attachments: [{ blob_id: 'sha256:UP1.t', filename: 'shot.png' }] },
    ]);
    expect(row.attachments).toEqual([
      { blob_id: 'sha256:UP1.t', mime_type: 'image/png', size: 10, filename: 'shot.png' },
    ]);

    patched.length = 0;
    screen.getAllByRole('button', { name: /Remove shot\.png/ })[0].click();
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(patched).toEqual([{ attachments: [] }]);
    expect(row.attachments).toEqual([]);
  });
});
