import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { IssueDetailPage } from './IssueDetailPage';
import type { Issue, IssueRun } from './boardModel';

// What a reducer test cannot reach: that the retry button offers a run only
// when the server would take one — the card's refusals (`retryRejection`)
// and the ledger's (`unsettledRun`) both reaching the same control. The two
// predicates' own cases live in `boardModel.test.ts`; these are about the
// affordance — gone, or disabled and saying why.

const PROJECT_ID = '01JPROJECT';

function issue(overrides: Partial<Issue> = {}): Issue {
  return {
    number: 7,
    project_id: PROJECT_ID,
    title: 'Wire the retry',
    description: '',
    status: 'in_progress',
    priority: 'none',
    position: 0,
    stage: 0,
    assignee: 'dev-1',
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

function run(status: IssueRun['status']): IssueRun {
  return {
    number: 7,
    attempt: 1,
    agent_id: 'dev-1',
    status,
    trigger: 'started',
    created_at_ms: 0,
  };
}

// One run, settled: the button reads "Run it again" and the card takes
// another — the unsettled states are the cases below.
const RUNS: IssueRun[] = [run('failed')];

const TEAM = [
  {
    id: 'dev-1',
    handle: 'dev-1',
    name: 'Dev',
    description: '',
    framework: 'baybo' as const,
    lead: false,
    created_at_ms: 0,
  },
];

const ok = { status: 200, ok: true } as Response;

let current: Issue = issue();
let currentRuns: IssueRun[] = RUNS;

const client = {
  GET: vi.fn(async (path: string) => {
    if (path === '/v1/projects/{project_id}/issues/{number}') {
      return { data: current, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues') {
      return { data: { items: [current] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/runs') {
      return { data: { items: currentRuns }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/events') {
      return { data: { items: [] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/agents') {
      return { data: { items: TEAM }, error: undefined, response: ok };
    }
    throw new Error(`unexpected GET ${path}`);
  }),
  POST: vi.fn(async () => ({ data: RUNS[0], error: undefined, response: ok })),
  PATCH: vi.fn(),
};

// Stable values, as the real context gives — a fresh object per call would
// re-run the load effect on every render.
const auth = { logout: vi.fn() };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

function renderIssue(row: Issue, runs: IssueRun[] = RUNS) {
  current = row;
  currentRuns = runs;
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT_ID}/issues/${row.number}`]}>
      <Routes>
        <Route path="/projects/:pid/issues/:num" element={<IssueDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

const retryButton = () => screen.findByRole('button', { name: 'Run it again' });

describe('IssueDetailPage retry', () => {
  beforeEach(() => {
    client.POST.mockClear();
  });

  it('offers a run on a card the board is still working', async () => {
    renderIssue(issue());

    expect(await retryButton()).toBeEnabled();
  });

  it('refuses a cancelled card in place, saying what the server would have said', async () => {
    renderIssue(issue({ cancelled_at_ms: 111 }));

    const button = await retryButton();
    expect(button).toBeDisabled();
    // Still on the page, and next to the way out — a button that vanished
    // would leave the operator hunting for the reason.
    expect(
      screen.getByText('this issue was cancelled — reopen it before running it again'),
    ).toBeInTheDocument();

    await userEvent.click(button);
    expect(client.POST).not.toHaveBeenCalled();
  });

  it('refuses a finished card, and names the other way back', async () => {
    renderIssue(issue({ status: 'done' }));

    expect(await retryButton()).toBeDisabled();
    expect(
      screen.getByText('this issue is done — move it back into the board before running it again'),
    ).toBeInTheDocument();
  });

  it('refuses a card nobody is on', async () => {
    renderIssue(issue({ assignee: null }));

    expect(await retryButton()).toBeDisabled();
    expect(screen.getByText('an issue with nobody on it cannot be run')).toBeInTheDocument();
  });

  it('keeps the button working on a held run — pressing it is what releases the hold', async () => {
    renderIssue(issue(), [run('held')]);

    const button = await retryButton();
    // `retry_run` goes through `enqueue`, which releases what the ceiling
    // allows *before* it writes, so this press is the operator's one control
    // over a held card. Disabling it would leave an exhausted board offering
    // nothing at all.
    expect(button).toBeEnabled();
    expect(
      screen.getByText(
        'this run is held — the project is over its daily budget, and starts as soon as there is room',
      ),
    ).toBeInTheDocument();

    await userEvent.click(button);
    expect(client.POST).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}/runs/retry',
      { params: { path: { project_id: PROJECT_ID, number: 7 } } },
    );
  });

  it('says the card refusal, not the budget, when both are true', async () => {
    renderIssue(issue({ status: 'done' }), [run('held')]);

    // The server asks in this order too: `retry_run` refuses a finished card
    // before `enqueue` ever reaches the ledger, so the budget note would be
    // an answer the click never brings back.
    expect(await retryButton()).toBeDisabled();
    expect(
      screen.getByText('this issue is done — move it back into the board before running it again'),
    ).toBeInTheDocument();
  });

  it('hides the button while a run is in flight, where the run row answers instead', async () => {
    for (const status of ['queued', 'running'] as const) {
      const view = renderIssue(issue(), [run(status)]);
      // The run row is what says why — wait for it, or the absence below
      // is only the page still loading.
      expect(await screen.findByText(status)).toBeInTheDocument();
      expect(screen.queryByRole('button', { name: 'Run it again' })).toBeNull();
      view.unmount();
    }
  });

  it('still sends the retry when the card takes runs', async () => {
    renderIssue(issue({ status: 'review' }));

    await userEvent.click(await retryButton());
    expect(client.POST).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}/runs/retry',
      { params: { path: { project_id: PROJECT_ID, number: 7 } } },
    );
  });
});
