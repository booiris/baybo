import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { ProjectBoardPage } from './ProjectBoardPage';
import type { Issue, Project } from './boardModel';

/// A real drag, driven through the real dnd-kit, on the real page.
///
/// It exists for one failure that nothing smaller could catch. The board
/// previews a move by mutating its own state, and dnd-kit re-measures every
/// droppable whenever the set of them changes — so a preview changes the
/// geometry that decides the preview. While a cursor decides, that is a fixed
/// point. While the **dragged rectangle** decided — which it did wherever the
/// cursor was over no droppable, the band across the column headers and the
/// 12px between two columns — it was a loop that needed no pointer event to
/// keep going: one mouse move into that band produced 27 previews and React
/// threw "Maximum update depth exceeded" (#185) over the operator's board.
///
/// jsdom has no layout, so the fake one below is the whole point: rects are
/// derived from the **current DOM order**, which is the edge that closed the
/// loop. Left at jsdom's zeros the bug is invisible.
//
// Instrumentation: the page's `onDragOver` body is `resolveDrop` + `moveCard`,
// both imported from `./boardModel`, and every render of DndContext runs
// `boardCollisionDetection` from `./dropTarget`. Wrapping both modules records
// the whole exchange without touching a production file.

const trace = vi.hoisted(() => ({
  lines: [] as string[],
  resolves: 0,
  moves: 0,
  collisions: 0,
  overIds: [] as string[],
  cursorHits: [] as number[],
}));

vi.mock('./boardModel', async (importOriginal) => {
  const real = await importOriginal<typeof import('./boardModel')>();
  const shape = (board: import('./boardModel').Board) =>
    real.COLUMNS.map((s) => `${s}[${board[s].map((i) => i.number).join(',')}]`)
      .filter((s) => !s.endsWith('[]'))
      .join(' ');
  return {
    ...real,
    resolveDrop: (view: import('./boardModel').Board, activeId: string, overId: string | null) => {
      const out = real.resolveDrop(view, activeId, overId);
      trace.resolves += 1;
      trace.lines.push(
        `#${trace.resolves} onDragOver over=${String(overId)} board=${shape(view)} -> ${
          out === null ? 'null' : `${out.status} before=${String(out.before)}`
        }`,
      );
      return out;
    },
    moveCard: (board: import('./boardModel').Board, drop: import('./boardModel').Drop) => {
      const next = real.moveCard(board, drop);
      trace.moves += 1;
      trace.lines.push(`        move -> ${shape(next)}`);
      return next;
    },
  };
});

vi.mock('./dropTarget', async (importOriginal) => {
  const real = await importOriginal<typeof import('./dropTarget')>();
  const { pointerWithin } = await import('@dnd-kit/core');
  return {
    ...real,
    boardCollisionDetection: (args: Parameters<typeof real.boardCollisionDetection>[0]) => {
      const out = real.boardCollisionDetection(args);
      trace.collisions += 1;
      // Which of the two branches inside `boardCollisionDetection` answered:
      // the cursor one, or the `rectIntersection` fallback.
      trace.cursorHits.push(pointerWithin(args).length);
      trace.overIds.push(out.length === 0 ? 'none' : String(out[0].id));
      return out;
    },
  };
});

// ---------------------------------------------------------------------------
// Fixture + stub client, lifted from ProjectBoardPage.test.tsx.
// ---------------------------------------------------------------------------

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

const NAMES = ['Alpha', 'Bravo', 'Charlie', 'Delta', 'Echo', 'Foxtrot', 'Golf', 'Hotel'] as const;

// Real boards have cards of different heights (a blocked badge, an assignee
// row, two lines of title). `CARD_HEIGHT` below models that, keyed by number.
const ISSUES: Issue[] = [
  issue(1, { title: 'Alpha', status: 'backlog', position: 0 }),
  issue(2, { title: 'Bravo', status: 'backlog', position: 1 }),
  issue(3, { title: 'Charlie', status: 'backlog', position: 2 }),
  issue(5, { title: 'Echo', status: 'todo', position: 0 }),
  issue(6, { title: 'Foxtrot', status: 'todo', position: 1 }),
  issue(7, { title: 'Golf', status: 'todo', position: 2 }),
  issue(4, { title: 'Delta', status: 'in_progress', position: 0 }),
  issue(8, { title: 'Hotel', status: 'in_progress', position: 1 }),
];

/// Per-card heights, so the geometry is not a uniform grid.
let CARD_HEIGHT: Record<number, number> = {};
const DEFAULT_HEIGHT = 90;

const ok = { status: 200, ok: true } as Response;
const noContent = { status: 204, ok: true } as Response;

function stubClient() {
  return {
    GET: vi.fn(async (path: string) => {
      if (path === '/v1/projects') {
        return { data: { items: [PROJECT] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}') {
        return { data: PROJECT, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/issues') {
        return { data: { items: ISSUES }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/runs') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/agents') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/feed') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/attention') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      throw new Error(`unexpected GET ${path}`);
    }),
    POST: vi.fn(
      async (_path: string, _init?: { params?: unknown; body?: unknown }) => ({
        data: undefined,
        error: undefined,
        response: noContent,
      }),
    ),
    PATCH: vi.fn(async () => ({ data: undefined, error: undefined, response: noContent })),
  };
}

const client = stubClient();
const auth = { logout: vi.fn(), baseUrl: 'http://board.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

// ---------------------------------------------------------------------------
// A fake layout engine. jsdom measures every element as 0×0 at (0,0), which
// makes dnd-kit's collision detection degenerate. Rects are derived from the
// CURRENT DOM — column index, then the card's index among its siblings — so a
// re-order performed by `onDragOver` changes what the next measurement sees.
// That edge is what closes the feedback loop.
// ---------------------------------------------------------------------------

const COL_W = 200; // pitch, column to column
const COL_INNER = 188; // 12px gap between columns, as `gap-3` gives
const COL_LEFT = 4;
const COL_TOP = 56;
const COL_H = 600;
const LIST_TOP = 96; // under the column header
const LIST_H = 560;
const CARD_PAD = 8;
const CARD_W = 172;
const CARD_GAP = 8;

type Box = { x: number; y: number; width: number; height: number };

function isCardWrapper(el: Element): boolean {
  return el.firstElementChild?.tagName === 'ARTICLE';
}

function numberOf(wrapper: Element): number {
  const name = NAMES.find((n) => wrapper.textContent.includes(n));
  const found = ISSUES.find((row) => row.title === name);
  return found?.number ?? 0;
}

function heightOf(wrapper: Element): number {
  return CARD_HEIGHT[numberOf(wrapper)] ?? DEFAULT_HEIGHT;
}

/// The rect the drag overlay reports. dnd-kit measures the overlay node once,
/// at pickup, and uses it as the `collisionRect` — which is what
/// `rectIntersection` (the fallback when the cursor is in a gap between
/// columns) collides. Left at 0×0 that whole path is dead.
let overlayBox: Box = { x: 0, y: 0, width: 0, height: 0 };

/// Where the board would put this element if jsdom had layout.
function boxOf(el: Element): Box {
  const zero = { x: 0, y: 0, width: 0, height: 0 };
  const sections = Array.from(document.querySelectorAll('section'));
  const section = el.closest('section');
  if (section === null) {
    // The DragOverlay renders its card outside every column.
    return el.querySelector('article') !== null || el.tagName === 'ARTICLE' ? overlayBox : zero;
  }
  const ci = sections.indexOf(section);
  if (ci === -1) return zero;
  const left = COL_LEFT + ci * COL_W;
  // The column's droppable is the whole section — header, borders and all.
  if (el === section) return { x: left, y: COL_TOP, width: COL_INNER, height: COL_H };

  // Two children: the column header, then the card list under it.
  const list = section.children.item(1);
  if (list === null) return zero;
  if (el === list) return { x: left, y: LIST_TOP, width: COL_INNER, height: LIST_H };

  let cur: Element | null = el;
  while (cur !== null && cur.parentElement !== list) cur = cur.parentElement;
  if (cur === null) return zero;

  let y = LIST_TOP + CARD_PAD;
  for (const sibling of Array.from(list.children).filter(isCardWrapper)) {
    const height = heightOf(sibling);
    if (sibling === cur) return { x: left + CARD_PAD, y, width: CARD_W, height };
    y += height + CARD_GAP;
  }
  return zero;
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

/// The board's own picture of itself: the rendered column order.
function renderedColumns(): string {
  return Array.from(document.querySelectorAll('section'))
    .map((section) => {
      const label = section.querySelector('h2')?.textContent ?? '?';
      const list = section.children.item(1);
      const cards = Array.from(list === null ? [] : list.children)
        .filter(isCardWrapper)
        .map((el) => NAMES.find((n) => el.textContent.includes(n)) ?? '?');
      return `${label}[${cards.join(',')}]`;
    })
    .filter((s) => !s.endsWith('[]'))
    .join(' ');
}

// ---------------------------------------------------------------------------
// PointerEvent polyfill: jsdom ships none, and dnd-kit's PointerSensor listens
// for pointerdown/pointermove/pointerup and demands `isPrimary`.
// ---------------------------------------------------------------------------

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

function cardBox(title: string): { node: HTMLElement; box: Box } {
  const article = screen.getByText(title).closest('article');
  if (article === null) throw new Error(`no card for ${title}`);
  const wrapper = article.parentElement as HTMLElement;
  return { node: wrapper, box: boxOf(wrapper) };
}

function centre(title: string): { node: HTMLElement; x: number; y: number } {
  const { node, box } = cardBox(title);
  return { node, x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

function renderBoard() {
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT.id}`]}>
      <Routes>
        <Route path="/projects/:pid" element={<ProjectBoardPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

// ---------------------------------------------------------------------------

describe('dragging a card on the real board', () => {
  let restoreLayout: () => void;
  let errors: string[];
  let consoleError: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    trace.lines = [];
    trace.resolves = 0;
    trace.moves = 0;
    trace.collisions = 0;
    trace.overIds = [];
    trace.cursorHits = [];
    CARD_HEIGHT = {};
    errors = [];
    client.POST.mockClear();
    restoreLayout = installFakeLayout();
    (window as unknown as { PointerEvent: typeof FakePointerEvent }).PointerEvent = FakePointerEvent;
    consoleError = vi.spyOn(console, 'error').mockImplementation((...args: unknown[]) => {
      errors.push(args.map((a) => (a instanceof Error ? a.message : String(a))).join(' '));
    });
  });

  afterEach(() => {
    restoreLayout();
    consoleError.mockRestore();
  });

  function crashed(): string | null {
    return errors.find((line) => line.includes('Maximum update depth')) ?? null;
  }

  /// pointerdown on a card, then the 4px the activation constraint wants.
  function grab(title: string): { x: number; y: number } {
    const { node, x, y } = centre(title);
    overlayBox = boxOf(node);
    fireEvent(node, pointer('pointerdown', x, y));
    fireEvent(document, pointer('pointermove', x, y + 6));
    return { x, y };
  }

  /// Every test ends its drag. dnd-kit listens on the document, so a drag left
  /// in the air outlives the board that started it and answers the next test's
  /// pointer events from a tree nobody is looking at.
  async function release(x: number, y: number): Promise<void> {
    fireEvent(document, pointer('pointerup', x, y));
    await settle();
  }

  async function settle(ms = 30): Promise<void> {
    await new Promise((resolve) => {
      setTimeout(resolve, ms);
    });
  }

  it('sanity: the fake layout puts each card where the board would', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    const alpha = centre('Alpha');
    const bravo = centre('Bravo');
    const echo = centre('Echo');
    expect(alpha.y).toBe(LIST_TOP + CARD_PAD + DEFAULT_HEIGHT / 2);
    expect(bravo.y).toBe(alpha.y + DEFAULT_HEIGHT + CARD_GAP);
    expect(echo.x).toBe(alpha.x + COL_W);
    expect(renderedColumns()).toBe(
      'Backlog[Alpha,Bravo,Charlie] Todo[Echo,Foxtrot,Golf] In Progress[Delta,Hotel]',
    );
  });

  /// The crash, from the one gesture that produced it. `x = 200` is the 12px
  /// `gap-3` between Backlog (4…192) and Todo (204…392) — board that belongs to
  /// no droppable, so `pointerWithin` comes back empty. It used to fall through
  /// to `rectIntersection`, which ranks by overlap with the dragged rectangle
  /// rather than by the cursor: the rectangle straddles both columns there, and
  /// each preview re-flowed the columns under it into the next answer.
  ///
  /// The cursor is stationary from the single move onwards, so anything past a
  /// couple of decisions is the board arguing with itself.
  it('holds its preview when a move lands in the gap between two columns', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    const from = grab('Bravo');
    await settle();
    const beforeMove = trace.resolves;
    const held = renderedColumns();

    fireEvent(document, pointer('pointermove', 200, 120));
    await settle();

    console.info(
      [
        `grabbed Bravo at (${from.x},${from.y}), one move to (200,120)`,
        ...trace.lines,
        `onDragOver calls from that ONE move: ${trace.resolves - beforeMove}`,
        `DndContext renders (collision passes) during it: ${trace.collisions}`,
        `overId sequence: ${[...new Set(trace.overIds)].join(' , ')}`,
        `pointerWithin hits per collision pass: ${[...new Set(trace.cursorHits)].join(',')}`,
      ].join('\n'),
    );

    expect(crashed()).toBeNull();
    expect(trace.resolves - beforeMove).toBeLessThan(3);
    // The cursor is over nothing, and the preview is where it already was.
    expect(trace.cursorHits.at(-1)).toBe(0);
    expect(renderedColumns()).toBe(held);
    await release(200, 120);
  }, 60000);

  /// The band across the column headers is the other half of the same dead
  /// zone, and the bigger one: it runs the full width of the board, and it is
  /// exactly where the operator aims a card they want at the top of a column.
  /// The column's droppable now reaches from its top border to its bottom one,
  /// so aiming there aims at that column, and the card goes in at the top.
  it('drops into the column whose header the cursor is over', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    grab('Bravo');
    await settle();

    fireEvent(document, pointer('pointermove', 260, 70));
    await settle();

    console.info(`header band: onDragOver=${trace.resolves}, rendered=${renderedColumns()}`);
    expect(crashed()).toBeNull();
    expect(renderedColumns()).toBe(
      'Backlog[Alpha,Charlie] Todo[Bravo,Echo,Foxtrot,Golf] In Progress[Delta,Hotel]',
    );
    await release(260, 70);
  }, 60000);

  /// The control: the same drag, landing inside a column, settles after two
  /// decisions — and settled the same way before any of this was fixed.
  it('settles when the same move lands inside a column', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    grab('Bravo');
    await settle();
    const beforeMove = trace.resolves;

    const foxtrot = centre('Foxtrot');
    fireEvent(document, pointer('pointermove', foxtrot.x, foxtrot.y));
    await settle();

    console.info(
      [
        `control: one move to (${foxtrot.x},${foxtrot.y}), inside Todo`,
        ...trace.lines,
        `onDragOver calls from that ONE move: ${trace.resolves - beforeMove}`,
        `rendered: ${renderedColumns()}`,
      ].join('\n'),
    );
    expect(crashed()).toBeNull();
    expect(trace.resolves - beforeMove).toBeLessThan(5);
    await release(foxtrot.x, foxtrot.y);
  }, 60000);

  /// The board's main gesture, end to end: a card dragged out of one column
  /// and dropped in another changes status, and the request says so.
  it('changes a card\u2019s status when it is dropped in another column', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    grab('Bravo');
    await settle();

    const foxtrot = centre('Foxtrot');
    fireEvent(document, pointer('pointermove', foxtrot.x, foxtrot.y));
    await settle();
    await release(foxtrot.x, foxtrot.y);

    const posted = client.POST.mock.calls.filter(
      ([path]) => path === '/v1/projects/{project_id}/issues/{number}/move',
    );
    console.info(`dropped inside Todo: rendered=${renderedColumns()} posted=${JSON.stringify(posted)}`);
    expect(crashed()).toBeNull();
    expect(renderedColumns()).toBe(
      'Backlog[Alpha,Charlie] Todo[Echo,Bravo,Foxtrot,Golf] In Progress[Delta,Hotel]',
    );
    expect(posted).toHaveLength(1);
    expect(posted[0][1]).toMatchObject({
      params: { path: { number: 2 } },
      body: { status: 'todo', ordered_numbers: [5, 2, 6, 7] },
    });
  }, 60000);

  /// Released over nothing, the preview is what gets written. A card that
  /// spent the whole drag sitting in Todo and then snapped back to Backlog
  /// because the button came up over a 12px seam reads as the board having
  /// eaten the move.
  it('writes the preview when the button comes up over nothing', async () => {
    renderBoard();
    await screen.findByText('Alpha');
    grab('Bravo');
    await settle();

    const foxtrot = centre('Foxtrot');
    fireEvent(document, pointer('pointermove', foxtrot.x, foxtrot.y));
    await settle();
    const previewed = renderedColumns();

    fireEvent(document, pointer('pointermove', 200, 120));
    await settle();
    await release(200, 120);

    const posted = client.POST.mock.calls.filter(
      ([path]) => path === '/v1/projects/{project_id}/issues/{number}/move',
    );
    console.info(`released in the seam: rendered=${renderedColumns()} posted=${JSON.stringify(posted)}`);
    expect(crashed()).toBeNull();
    expect(renderedColumns()).toBe(previewed);
    expect(posted).toHaveLength(1);
    expect(posted[0][1]).toMatchObject({
      params: { path: { number: 2 } },
      body: { status: 'todo', ordered_numbers: [2, 5, 6, 7] },
    });
  }, 60000);
});
