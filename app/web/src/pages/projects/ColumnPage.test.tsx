import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { ColumnPage } from './ColumnPage';
import type { Issue, Project } from './boardModel';

const PROJECT: Project = {
  id: '01JPROJECT',
  name: 'Kanban',
  description: '',
  workdir: '/tmp/kanban',
  max_parallel_issue_runs: 3,
  created_at_ms: 0,
  updated_at_ms: 0,
};

function issue(number: number, overrides: Partial<Issue> = {}): Issue {
  return {
    number,
    project_id: PROJECT.id,
    title: `issue ${number}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: 0,
    stage: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    unread: 0,
    last_run_failed: false,
    pinned: false,
    ...overrides,
  };
}

const ISSUES: Issue[] = [
  issue(1, { title: 'Wire the board', position: 0, assignee: 'dev-1' }),
  issue(2, { title: 'Blocked one', position: 1, blocked_reason: 'waiting on tmux', unread: 2 }),
  issue(3, { title: 'Cancelled one', position: 2, cancelled_at_ms: 111 }),
  issue(4, {
    title: 'Under way',
    status: 'in_progress',
    position: 0,
    priority: 'urgent',
    assignee: 'dev-1',
    // News in a stage that is not on screen — what the tab's dot is for.
    unread: 1,
  }),
];

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
const refused = { status: 500, ok: false } as Response;

/// Swapped per test: the archived variant, the fail-once move.
let projectDto: Project = PROJECT;
let failMoves = 0;

function stubClient() {
  return {
    GET: vi.fn(async (path: string) => {
      if (path === '/v1/projects/{project_id}') {
        return { data: projectDto, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/issues') {
        return { data: { items: ISSUES }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/runs') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/agents') {
        return { data: { items: TEAM }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/attention') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/llm/models') {
        return {
          data: { items: [{ name: 'model-1' }], default_name: 'model-1' },
          error: undefined,
          response: ok,
        };
      }
      throw new Error(`unexpected GET ${path}`);
    }),
    POST: vi.fn(async (path: string, init: unknown) => {
      if (path === '/v1/projects/{project_id}/issues/{number}/move') {
        if (failMoves > 0) {
          failMoves -= 1;
          return { data: undefined, error: { error: 'the server said no' }, response: refused };
        }
        const body = (init as { body: { status: Issue['status'] } }).body;
        return { data: issue(0, { status: body.status }), error: undefined, response: ok };
      }
      throw new Error(`unexpected POST ${path}`);
    }),
    // Resolved, not bare: `patchIssue` destructures the answer, and a stub
    // returning `undefined` throws inside it — which the client reports as a
    // failed write, so every optimistic press would silently roll back.
    PATCH: vi.fn(async () => ({ data: undefined, error: undefined, response: ok })),
  };
}

const client = stubClient();

/// The stream is mocked so a test can hand the page a `project_changed`
/// frame; the real hook is a no-op here anyway (no token, no socket).
const stream = vi.hoisted(() => ({ onFrame: undefined as (() => void) | undefined }));
vi.mock('./useBoardStream', () => ({
  useBoardStream: (_projectId: string, _issue: number | null, onChange: () => void) => {
    stream.onFrame = onChange;
  },
}));

const auth = { logout: vi.fn(), baseUrl: 'http://column.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

function renderColumn(status = 'backlog', query = '') {
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT.id}/board/${status}${query}`]}>
      <Routes>
        <Route path="/projects/:pid" element={<p>the whole board</p>} />
        <Route path="/projects/:pid/board/:status" element={<ColumnPage />} />
        <Route path="/projects/:pid/issues/:num" element={<p>issue detail</p>} />
      </Routes>
    </MemoryRouter>,
  );
}

// ---------------------------------------------------------------------------
// A fake layout for the drag tests, after `boardDrag.test.tsx`: jsdom
// measures everything as 0×0, so rects are derived from the CURRENT DOM
// order of the rows in the list. One column, so one stack of uniform rows.
// ---------------------------------------------------------------------------

const ROW_TOP = 100;
const ROW_H = 40;
const ROW_W = 800;

type Box = { x: number; y: number; width: number; height: number };

/// The rect the drag overlay reports — measured once, at pickup.
let overlayBox: Box = { x: 0, y: 0, width: 0, height: 0 };

function listContainer(): Element | null {
  return document.querySelector('[class*="divide-y"]');
}

function boxOf(el: Element): Box {
  const zero = { x: 0, y: 0, width: 0, height: 0 };
  const container = listContainer();
  if (container === null) return zero;
  if (el === container) {
    return { x: 0, y: ROW_TOP, width: ROW_W, height: container.children.length * ROW_H };
  }
  if (container.contains(el)) {
    let cur: Element | null = el;
    while (cur !== null && cur.parentElement !== container) cur = cur.parentElement;
    if (cur === null) return zero;
    const index = Array.from(container.children).indexOf(cur);
    return { x: 0, y: ROW_TOP + index * ROW_H, width: ROW_W, height: ROW_H };
  }
  // Page shells that contain the list measure zero; what is left outside it
  // is the drag overlay.
  if (el.contains(container)) return zero;
  return overlayBox;
}

function installFakeLayout(): () => void {
  const original = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function fake(this: Element): DOMRect {
    const b = boxOf(this);
    return {
      ...b,
      top: b.y,
      left: b.x,
      right: b.x + b.width,
      bottom: b.y + b.height,
      toJSON: () => b,
    } as DOMRect;
  };
  return () => {
    Element.prototype.getBoundingClientRect = original;
  };
}

class FakePointerEvent extends MouseEvent {
  pointerId = 1;
  pointerType = 'mouse';
  isPrimary = true;
  width = 1;
  height = 1;
  pressure = 0.5;
}

function pointer(type: string, x: number, y: number): FakePointerEvent {
  return new FakePointerEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: x,
    clientY: y,
    button: 0,
    buttons: 1,
  });
}

function rowNode(title: string): HTMLElement {
  const container = listContainer();
  let el: Element | null = screen.getByTitle(title);
  while (el !== null && el.parentElement !== container) el = el.parentElement;
  if (el === null) throw new Error(`no row for ${title}`);
  return el as HTMLElement;
}

function rowCentre(title: string): { node: HTMLElement; x: number; y: number } {
  const node = rowNode(title);
  const box = boxOf(node);
  return { node, x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

async function settle(ms = 30): Promise<void> {
  await act(async () => {
    await new Promise((resolve) => {
      setTimeout(resolve, ms);
    });
  });
}

/// pointerdown on a row, then the 4px the activation constraint wants.
function grab(title: string): { x: number; y: number } {
  const { node, x, y } = rowCentre(title);
  overlayBox = boxOf(node);
  fireEvent(node, pointer('pointerdown', x, y));
  fireEvent(document, pointer('pointermove', x, y + 6));
  return { x, y };
}

function movesPosted(): unknown[][] {
  return client.POST.mock.calls.filter(
    ([path]) => path === '/v1/projects/{project_id}/issues/{number}/move',
  );
}

describe('ColumnPage', () => {
  beforeEach(() => {
    projectDto = PROJECT;
    failMoves = 0;
    stream.onFrame = undefined;
    client.POST.mockClear();
    client.PATCH.mockClear();
    client.GET.mockClear();
  });

  it('shows only the routed column, one row per card, unread hoisted on top', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The other columns' cards are on the tabs, not in the list.
    expect(screen.queryByText('Under way')).not.toBeInTheDocument();

    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    // #2 carries something new, so it reads first — without `position`
    // being touched (the move that would write it is never sent here).
    expect(rows[0].textContent).toContain('Blocked one');
    expect(rows[1].textContent).toContain('Wire the board');
    expect(screen.getByText('⚑ Blocked')).toBeInTheDocument();
    const cancelled = screen.getByTitle('Cancelled one');
    expect(cancelled.className).toContain('line-through');
  });

  it('keeps every stage one press away, with the board’s own counts', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const tabs = screen.getByRole('navigation', { name: 'Stages' });
    const active = within(tabs).getByText('BACKLOG').closest('a');
    expect(active?.getAttribute('aria-current')).toBe('page');
    // 2 live in backlog (the cancelled card is not live), 1 in progress.
    expect(within(tabs).getByText('IN PROG').closest('a')?.textContent).toContain('1');
    expect(active?.textContent).toContain('2');

    await userEvent.click(within(tabs).getByText('IN PROG'));
    expect(await screen.findByText('Under way')).toBeInTheDocument();
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
  });

  it('wears a dot for another stage holding something new, never for its own', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // #4 in In Progress is unread; the dot is the only sign of it here. The
    // open stage's news is its rows' own pills, so its tab stays bare.
    expect(screen.getByTitle('Something new in In Progress')).toBeInTheDocument();
    expect(screen.queryByTitle('Something new in Backlog')).not.toBeInTheDocument();
  });

  it('bounces an unknown stage back to the board', async () => {
    renderColumn('shipping');
    expect(await screen.findByText('the whole board')).toBeInTheDocument();
  });

  it('narrows from the same URL vocabulary the board reads', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    // …and the tab admits the filter is holding cards back.
    const tabs = screen.getByRole('navigation', { name: 'Stages' });
    expect(within(tabs).getByText('BACKLOG').closest('a')?.textContent).toContain('1/2');
    expect(screen.getByLabelText('Filter the board (1 active)')).toBeInTheDocument();
  });

  it('opens the card from its row', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByText('Wire the board'));
    expect(await screen.findByText('issue detail')).toBeInTheDocument();
  });

  it('opens the assignee’s profile from the row without opening the card', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getAllByTitle(/Open @dev-1's profile/)[0]);
    expect(
      await screen.findByRole('button', { name: /Close the agent profile/ }),
    ).toBeInTheDocument();
    expect(screen.queryByText('issue detail')).not.toBeInTheDocument();
  });

  it('moves a card to another column from its row, joining the end of the queue', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Status of #1: Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    await waitFor(() => {
      expect(client.POST).toHaveBeenCalledWith(
        '/v1/projects/{project_id}/issues/{number}/move',
        expect.objectContaining({
          body: { status: 'in_progress', ordered_numbers: [4, 1] },
        }),
      );
    });
    // The row left this stage's list…
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    // …and the drop that starts an agent is the one move that says so.
    expect(await screen.findByText('Queued for @dev-1 — #1')).toBeInTheDocument();
  });

  it('opens the status picker on Enter instead of lifting the row', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The row is the keyboard sensor's activator, so a keydown bubbling out
    // of the picker's trigger must press the trigger — not start a silent
    // keyboard drag whose next Enter posts a reorder nobody aimed.
    screen.getByLabelText('Status of #1: Backlog').focus();
    await userEvent.keyboard('{Enter}');

    expect(await screen.findByRole('button', { name: 'In Progress' })).toBeInTheDocument();
  });

  it('refuses to start an unassigned card, out loud', async () => {
    renderColumn();
    await screen.findByText('Blocked one');
    const posts = client.POST.mock.calls.length;

    await userEvent.click(screen.getByLabelText('Status of #2: Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    // The whole sentence: the column named is the one the card snapped BACK
    // to, not the one that refused it.
    expect(
      await screen.findByText('Assign an agent first — #2 is back in Backlog'),
    ).toBeInTheDocument();
    // The card stayed put and nothing was sent.
    expect(screen.getByText('Blocked one')).toBeInTheDocument();
    expect(client.POST.mock.calls.length).toBe(posts);
  });

  it('rolls a failed move back, out loud, and Retry sends it again', async () => {
    failMoves = 1;
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Status of #1: Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    expect(
      await screen.findByText('Move failed, rolled back — the server said no'),
    ).toBeInTheDocument();
    // Rolled back: the row is still in this stage's list.
    expect(screen.getByText('Wire the board')).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => {
      expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    });
    expect(movesPosted()).toHaveLength(2);
  });

  it('opens the create modal on this column', async () => {
    renderColumn('review');
    await screen.findByTitle('New issue in Review');

    await userEvent.click(screen.getByTitle('New issue in Review'));
    await waitFor(() => {
      expect(screen.getByPlaceholderText('Issue title')).toBeInTheDocument();
    });
  });

  it('pins a row to the top of the column, the same press the board takes', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    await userEvent.click(
      within(rows[1].closest('div[class*="group"]') as HTMLElement).getByRole('button', {
        name: /Pin this card/,
      }),
    );

    expect(client.PATCH).toHaveBeenCalledWith('/v1/projects/{project_id}/issues/{number}', {
      params: { path: { project_id: PROJECT.id, number: 1 } },
      body: { pinned: true },
    });
    await waitFor(() => {
      const after = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
      expect(after[0].textContent).toContain('Wire the board');
    });
    // The row's own click opens the card; the pin must claim its press.
    expect(screen.queryByText('issue detail')).not.toBeInTheDocument();
  });

  it('reads an archived project without offering to work it', async () => {
    projectDto = { ...PROJECT, archived_at_ms: 111 };
    renderColumn();
    await screen.findByText('Wire the board');

    expect(screen.getByText('Archived — read only')).toBeInTheDocument();
    expect(screen.getByTitle('New issue in Backlog')).toBeDisabled();
    // The status chip is a fact, not a control.
    expect(screen.queryByLabelText('Status of #1: Backlog')).not.toBeInTheDocument();
  });

  it('walks back to the board without dropping the narrowing', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    const back = screen.getByText('Board').closest('a');
    expect(back?.getAttribute('href')).toBe(`/projects/${PROJECT.id}?q=blocked`);
  });

  describe('dragging a row', () => {
    let restoreLayout: () => void;

    beforeEach(() => {
      restoreLayout = installFakeLayout();
      (window as unknown as { PointerEvent: typeof FakePointerEvent }).PointerEvent =
        FakePointerEvent;
    });

    afterEach(() => {
      restoreLayout();
    });

    it('a drag writes the stored order, not the reading order — twice in a row', async () => {
      renderColumn();
      await screen.findByText('Wire the board');
      // Reading order: [Blocked #2 (unread), Wire #1, Cancelled #3].
      // Stored positions: #1@0, #2@1, #3@2.

      // Drag the bottom row onto the top slot.
      grab('Cancelled one');
      await settle();
      const to = rowCentre('Blocked one');
      fireEvent(document, pointer('pointermove', to.x, to.y));
      await settle();
      fireEvent(document, pointer('pointerup', to.x, to.y));

      // Inserted before #2 in the STORED order — [1,3,2], not the rendered
      // [3,2,1]: the reading order is never written.
      await waitFor(() => {
        expect(movesPosted()).toHaveLength(1);
      });
      expect(movesPosted()[0][1]).toMatchObject({
        body: { status: 'backlog', ordered_numbers: [1, 3, 2] },
      });

      // A second drag BEFORE any refetch: the first drag's order was written
      // back onto the cards, so this one anchors against [1,3,2] — stale
      // positions would send [2,1,3] and undo the first move.
      grab('Wire the board');
      await settle();
      const top = rowCentre('Cancelled one');
      fireEvent(document, pointer('pointermove', top.x, top.y));
      await settle();
      fireEvent(document, pointer('pointerup', top.x, top.y));

      await waitFor(() => {
        expect(movesPosted()).toHaveLength(2);
      });
      expect(movesPosted()[1][1]).toMatchObject({
        body: { status: 'backlog', ordered_numbers: [1, 3, 2] },
      });
    });

    it('holds a frame while a row is in the air, and answers it on landing', async () => {
      renderColumn();
      await screen.findByText('Wire the board');
      const issueGets = () =>
        client.GET.mock.calls.filter(([path]) => path === '/v1/projects/{project_id}/issues')
          .length;
      const before = issueGets();

      const from = grab('Wire the board');
      await settle();
      // A `project_changed` frame lands mid-drag: it must NOT refetch under
      // the drag — a refetch re-keys every row and re-ranks the very list
      // being dragged in.
      act(() => {
        stream.onFrame?.();
      });
      await settle();
      expect(issueGets()).toBe(before);

      // Released in place: no move posted, and the held frame is answered.
      fireEvent(document, pointer('pointerup', from.x, from.y + 6));
      await waitFor(() => {
        expect(issueGets()).toBe(before + 1);
      });
      expect(movesPosted()).toHaveLength(0);
    });
  });
});
