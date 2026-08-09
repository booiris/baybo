import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { LeadPanel } from './LeadPanel';


const api = vi.hoisted(() => ({
  fetchLeadConversations: vi.fn(),
  fetchLeadMessages: vi.fn(),
  openLeadConversation: vi.fn(),
}));
const socket = vi.hoisted(() => ({ sendMessage: vi.fn(), close: vi.fn(), subscribed: [] as string[] }));

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

function conversation(id: string, lastActiveMs: number) {
  return { session_id: id, last_active_ms: lastActiveMs, created_at_ms: 0 };
}

function renderPanel(readOnly = false) {
  const onBoardChanged = vi.fn();
  render(
    <LeadPanel
      projectId="01JP"
      readOnly={readOnly}
      onClose={vi.fn()}
      onBoardChanged={onBoardChanged}
    />,
  );
  return onBoardChanged;
}

describe('LeadPanel', () => {
  beforeEach(() => {
    api.fetchLeadConversations.mockReset().mockResolvedValue({ kind: 'ok', value: [] });
    api.fetchLeadMessages.mockReset().mockResolvedValue({ kind: 'ok', value: [] });
    api.openLeadConversation.mockReset();
    socket.sendMessage.mockReset();
    socket.close.mockReset();
    socket.subscribed = [];
  });

  it('says what the panel is for when the board has no conversations', async () => {
    renderPanel();
    expect(await screen.findByText(/turns what you agree into cards/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
  });

  it('opens on the most recently active conversation and loads its history', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-new', 200), conversation('board-old', 100)],
    });
    api.fetchLeadMessages.mockResolvedValue({
      kind: 'ok',
      value: [
        { id: 'm1', kind: 'message', role: 'user', text: 'plan the parser', at: '' },
        { id: 'm2', kind: 'work', role: 'assistant', text: 'opened #4', at: '' },
      ],
    });
    renderPanel();

    expect(await screen.findByText('plan the parser')).toBeInTheDocument();
    expect(screen.getByText('opened #4')).toBeInTheDocument();
    await waitFor(() => {
      expect(api.fetchLeadMessages).toHaveBeenCalledWith(client, 'board-new');
    });
    expect(socket.subscribed).toEqual(['board-new']);
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

  it('starts a conversation and selects it', async () => {
    api.openLeadConversation.mockResolvedValue({
      kind: 'ok',
      value: conversation('board-fresh', 0),
    });
    api.fetchLeadConversations
      .mockResolvedValueOnce({ kind: 'ok', value: [] })
      .mockResolvedValue({ kind: 'ok', value: [conversation('board-fresh', 0)] });
    renderPanel();

    await userEvent.click(await screen.findByRole('button', { name: /Start a new conversation/ }));
    await waitFor(() => {
      expect(socket.subscribed).toEqual(['board-fresh']);
    });
  });

  it('offers no writes on an archived board', async () => {
    api.fetchLeadConversations.mockResolvedValue({
      kind: 'ok',
      value: [conversation('board-1', 0)],
    });
    renderPanel(true);
    await screen.findByLabelText('Conversation');
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
});
