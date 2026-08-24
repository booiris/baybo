import { useEffect, useRef } from 'react';

import { ChatWs, type Frame } from '../../api/chatWs';
import { useAuth } from '../../api/auth';

export function wantsRefresh(
  frame: Frame,
  projectId: string,
  issueNumber: number | null,
): boolean {
  if (frame.kind !== 'project_changed') return false;
  if (frame.project_id !== projectId) return false;
  // The board takes timeline frames too. It used to skip them on the
  // grounds that it draws no timeline — true until a card started carrying
  // its own unread count, which changes on exactly those frames and on no
  // other. Skipping them left the one number that says "an agent is asking
  // you something" stale for as long as the operator kept looking at it.
  if (issueNumber === null) return true;
  return frame.issue_number === undefined || frame.issue_number === issueNumber;
}

export function useBoardStream(
  projectId: string,
  issueNumber: number | null,
  onChange: () => void,
): void {
  // Keep the callback fresh without reconnecting the socket on every render.
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
  }, [token, baseUrl, projectId, issueNumber]);
}
