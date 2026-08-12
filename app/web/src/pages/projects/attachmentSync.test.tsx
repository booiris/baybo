import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useNavigate } from 'react-router-dom';

import { IssueDetailPage } from './IssueDetailPage';
import type { Issue } from './boardModel';

const PROJECT_ID = '01JPROJECT';

const ok = { status: 200, ok: true } as Response;

function issue(number: number, attachments: Issue['attachments']): Issue {
  return {
    number,
    project_id: PROJECT_ID,
    title: `card ${String(number)}`,
    description: '',
    attachments,
    status: 'backlog',
    priority: 'none',
    position: 0,
    stage: 0,
    assignee: null,
    created_at_ms: 0,
    updated_at_ms: 0,
  } as unknown as Issue;
}

const CARDS: Record<number, Issue> = {
  7: issue(7, [{ blob_id: 'sha256:AAA.t', mime_type: 'image/png', size: 10, filename: 'a.png' }]),
  8: issue(8, [{ blob_id: 'sha256:BBB.t', mime_type: 'image/png', size: 10, filename: 'b.png' }]),
};

const patched: unknown[] = [];

const client = {
  GET: vi.fn(async (path: string, opts?: { params?: { path?: { number?: number } } }) => {
    const number = opts?.params?.path?.number ?? 7;
    if (path === '/v1/projects/{project_id}/issues/{number}') {
      return { data: CARDS[number], error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues') {
      return { data: { items: Object.values(CARDS) }, error: undefined, response: ok };
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
  PATCH: vi.fn(async (_path: string, init: { params: { path: { number: number } }; body: unknown }) => {
    patched.push({ number: init.params.path.number, body: init.body });
    return { data: CARDS[init.params.path.number], error: undefined, response: ok };
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

vi.stubGlobal(
  'fetch',
  vi.fn(() => new Promise(() => undefined)),
);

function Jump() {
  const navigate = useNavigate();
  return (
    <button
      type="button"
      onClick={() => {
        navigate(`/projects/${PROJECT_ID}/issues/8`);
      }}
    >
      go to 8
    </button>
  );
}

/// Two hazards that both write a card's files over another card's, and both
/// come from the same place: this page does **not** unmount when the route
/// parameter changes, so a draft seeded for one card is still mounted for the
/// next — and a save driven by "the draft and the row disagree" fires during
/// the window where the row on screen is still the previous card's.
describe('the description draft against the card it belongs to', () => {
  it('does not carry the first card’s files onto the second', async () => {
    render(
      <MemoryRouter initialEntries={[`/projects/${PROJECT_ID}/issues/7`]}>
        <Jump />
        <Routes>
          <Route path="/projects/:pid/issues/:num" element={<IssueDetailPage />} />
        </Routes>
      </MemoryRouter>,
    );
    await screen.findAllByText('card 7');
    screen.getByRole('button', { name: 'go to 8' }).click();
    await screen.findAllByText('card 8');
    // Long enough for the seed and the save effects to have run and for any
    // upload-shaped promise to have settled; the point is that nothing was
    // written, so the wait has to outlast the write that would be wrong.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(patched).toEqual([]);
  });

  it('writes nothing at all when a card with files is merely opened', async () => {
    render(
      <MemoryRouter initialEntries={[`/projects/${PROJECT_ID}/issues/7`]}>
        <Routes>
          <Route path="/projects/:pid/issues/:num" element={<IssueDetailPage />} />
        </Routes>
      </MemoryRouter>,
    );
    await screen.findAllByText('card 7');
    // The narrow window this guards: on the commit where the row lands, the
    // draft has been *told* to seed but has not yet seeded, so a save driven
    // off "they disagree" would see an empty draft against a card with files
    // and clear the card. Reading a card must never write to it.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(patched).toEqual([]);
  });
});
