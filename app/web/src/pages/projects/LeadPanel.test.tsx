import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { LeadPanel } from './LeadPanel';

const api = vi.hoisted(() => ({
  fetchLeadConversations: vi.fn(),
  fetchLeadMessages: vi.fn(),
  openLeadConversation: vi.fn(),
}));
const socket = vi.hoisted(() => ({
  sendMessage: vi.fn(),
  close: vi.fn(),
  subscribed: [] as string[],
}));

const client = vi.hoisted(() => ({}));
const auth = vi.hoisted(() => ({ token: 'tok', baseUrl: 'http://x', logout: () => {} }));

vi.mock('./api', () => api);
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));
vi.mock('../../api/chatWs', () => ({
  ChatWs: class {
    constructor(opts: { initialSessionIds?: string[] }) {
      socket.subscribed = opts.initialSessionIds ?? [];
    }
    sendMessage = socket.sendMessage;
    close = socket.close;
  },
}));

function conversation(id: string, lastActiveMs: number, title?: string) {
  return { session_id: id, last_active_ms: lastActiveMs, created_at_ms: 0, title };
}

function history(rows: unknown[], hasMoreOlder = false, oldestOrdinal: number | null = null) {
  return { kind: 'ok', value: { rows, hasMoreOlder, oldestOrdinal } };
}

function renderPanel(readOnly = false) {
  const onBoardChanged = vi.fn();
  const onHireLead = vi.fn();
  render(
    <MemoryRouter>
      <LeadPanel
        projectId="01JP"
        projectName="baybo"
        readOnly={readOnly}
        onClose={vi.fn()}
        onBoardChanged={onBoardChanged}
        onHireLead={onHireLead}
      />
    </MemoryRouter>,
  );
  return onBoardChanged;
}

describe('LeadPanel', () => {
  beforeEach(() => {
    api.fetchLeadConversations.mockReset().mockResolvedValue({ kind: 'ok', value: [] });
    api.fetchLeadMessages.mockReset().mockResolvedValue(history([]));
    api.openLeadConversation.mockReset();
    socket.sendMessage.mockReset();
    socket.close.mockReset();
    socket.subscribed = [];
  });

  it('offers the composer before any conversation exists', async () => {
    // The first conversation is created lazily, on the first message. With
    // the composer hidden until one existed, a board whose lead lookup was
    // broken had no way in at all.
    renderPanel();
    expect(await screen.findByText(/turns what you agree into cards/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Send' })).toBeInTheDocument();
  });

  it('creates the conversation on the first message rather than on open', async () => {
    api.openLeadConversation.mockResolvedValue({
      kind: 'ok',
      value: conversation('board-lazy', 0),
    });
    api.fetchLeadConversations
      .mockResolvedValueOnce({ kind: 'ok', value: [] })
      .mockResolvedValue({ kind: 'ok', value: [conversation('board-lazy', 0)] });
    renderPanel();

    const box = await screen.findByPlaceholderText(/Ask the lead/);
    expect(api.openLeadConversation).not.toHaveBeenCalled();

    await userEvent.type(box, 'plan the parser');
    await userEvent.click(screen.getByRole('button', { name: 'Send' }));

    await waitFor(() => {
      expect(api.openLeadConversation).toHaveBeenCalledWith(client, '01JP');
    });
  });

  it('names the board it belongs to', async () => {
    renderPanel();
    expect(await screen.findByText('baybo · lead')).toBeInTheDocument();
  });

  it('opens on the most recently active conversation and loads its history', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-new', 200), conversation('board-old', 100)],
    });
    api.fetchLeadMessages.mockResolvedValue(
      history([
        { id: 'm1', kind: 'message', role: 'user', text: 'plan the parser', created_at: '' },
      ]),
    );
    renderPanel();

    expect(await screen.findByText('plan the parser')).toBeInTheDocument();
    await waitFor(() => {
      expect(api.fetchLeadMessages).toHaveBeenCalledWith(client, 'board-new');
    });
    expect(socket.subscribed).toEqual(['board-new']);
  });

  it('shows the lead’s board actions as cards that link to the card', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-1', 0)],
    });
    api.fetchLeadMessages.mockResolvedValue(
      history([
        {
          id: 'w1',
          kind: 'work',
          role: 'assistant',
          text: '',
          created_at: '',
          steps: [
            { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #16' },
            { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #17' },
          ],
        },
      ]),
    );
    renderPanel();

    // Two consecutive calls of the same kind aggregate rather than stacking.
    expect(await screen.findByText(/created ×2/)).toBeInTheDocument();
    expect(screen.getByRole('link', { name: '#16' })).toHaveAttribute(
      'href',
      '/projects/01JP/issues/16',
    );
    expect(screen.getByRole('link', { name: '#17' })).toBeInTheDocument();
  });

  it('collapses a turn’s machinery behind a Worked label it can expand', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-1', 0)],
    });
    api.fetchLeadMessages.mockResolvedValue(
      history([
        {
          id: 'w1',
          kind: 'work',
          role: 'assistant',
          text: '',
          created_at: '',
          work_started_at: '2026-08-09T00:00:00.000Z',
          work_ended_at: '2026-08-09T00:00:09.000Z',
          steps: [{ kind: 'tool', tool: 'IssueList', tool_summary: 'read the board' }],
        },
      ]),
    );
    renderPanel();

    const summary = await screen.findByText(/Worked 9s/);
    expect(screen.queryByText(/read the board/)).not.toBeInTheDocument();
    await userEvent.click(summary);
    expect(screen.getByText(/read the board/)).toBeInTheDocument();
  });

  it('sends to the selected conversation and clears the composer', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-1', 0)],
    });
    renderPanel();
    const box = await screen.findByPlaceholderText(/Ask the lead/);

    await userEvent.type(box, 'split this into stages');
    await userEvent.click(screen.getByRole('button', { name: 'Send' }));

    expect(socket.sendMessage).toHaveBeenCalledWith({
      sessionId: 'board-1',
      userId: 'owner',
      content: 'split this into stages',
    });
    expect(box).toHaveValue('');
    expect(screen.getByText('split this into stages')).toBeInTheDocument();
  });

  it('picks a conversation out of the history list', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-new', 200, 'Observability plan'), conversation('board-old', 100)],
    });
    renderPanel();

    await userEvent.click(await screen.findByRole('button', { name: 'Conversation history' }));
    expect(screen.getByText('Observability plan')).toBeInTheDocument();
    await userEvent.click(screen.getByText('Conversation 1'));
    await waitFor(() => {
      expect(socket.subscribed).toEqual(['board-old']);
    });
  });

  it('offers no writes on an archived board', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-1', 0)],
    });
    renderPanel(true);
    await screen.findByText('baybo · lead');
    expect(
      screen.queryByRole('button', { name: /Start a new conversation/ }),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
  });

  it('shows why the panel is empty when the list cannot be read', async () => {
    api.fetchLeadConversations.mockResolvedValue({ kind: 'failed', message: 'HTTP Error 500' });
    renderPanel();
    expect(await screen.findByText('HTTP Error 500')).toBeInTheDocument();
  });

  it('explains a board with no lead, and offers the fix', async () => {
    // A legacy board: opened before the lead was seeded with the project.
    // The raw 404 said nothing about why, and hid that triage is dead too.
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'failed',
      message: 'not found: project 01JP has no lead',
    });
    renderPanel();

    expect(await screen.findByText(/This board has no lead/)).toBeInTheDocument();
    expect(screen.getByText(/not being triaged/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add one' })).toBeInTheDocument();
  });

  it('still shows an ordinary failure as itself', async () => {
    api.fetchLeadConversations.mockResolvedValue({ kind: 'failed', message: 'HTTP Error 503' });
    renderPanel();
    expect(await screen.findByText('HTTP Error 503')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add one' })).not.toBeInTheDocument();
  });
});