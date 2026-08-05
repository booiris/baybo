import { useEffect, useRef } from 'react';

import { ChatWs, type Frame } from '../../api/chatWs';
import { useAuth } from '../../api/auth';

/**
 * Which board changes a page cares about, decided as a pure function so it
 * can be tested without a socket.
 *
 * A page watches one project. It ignores frames for any other board, and a
 * detail page additionally ignores anything about a different card — but
 * never a frame with no issue number, since that is a board-wide change
 * (an archive, a reorder) that could still move what it is showing.
 */
export function wantsRefresh(
  frame: Frame,
  projectId: string,
  issueNumber: number | null,
): boolean {
  if (frame.kind !== 'project_changed') return false;
  if (frame.project_id !== projectId) return false;
  if (issueNumber === null) return true;
  return frame.issue_number === undefined || frame.issue_number === issueNumber;
}

/**
 * Refetch when this project changes.
 *
 * One owner-channel connection per board page. The routes are mutually
 * exclusive, so this is never live at the same time as the chat page's
 * connection, and the board subscribes to no session: everything it wants
 * is a session-less broadcast.
 */
export function useBoardStream(
  projectId: string,
  issueNumber: number | null,
  onChange: () => void,
): void {
  const handler = useRef(onChange);
  handler.current = onChange;
  const { token, baseUrl } = useAuth();

  useEffect(() => {
    if (token == null || projectId.length === 0) return;
    const ws = new ChatWs({
      baseUrl,
      adminToken: token,
      initialSessionIds: [],
      onFrame: (frame) => {
        if (wantsRefresh(frame, projectId, issueNumber)) handler.current();
      },
    });
    return () => {
      ws.close();
    };
    // `onChange` is deliberately not a dependency — it rides the ref, so a
    // parent re-render does not tear down the socket.
  }, [token, baseUrl, projectId, issueNumber]);
}
