/** Sidebar projection of a chat session. Shared between the ChatPage
 *  orchestrator and the extracted chat zone components. */
export interface SessionSummary {
  session_id: string;
  created_at: string;
  last_active: string;
  /** Local-only unread counter. Server doesn't surface this — the
   *  sidebar derives it from incoming `Frame::SessionActivity`. Cleared
   *  on navigation to the session. Always 0 on the row the user is
   *  currently viewing because activity for foreground sessions
   *  doesn't bump. */
  unread: number;
  /** Whether the user pinned this session to the top of the chat list.
   *  Pinned rows render in their own block above the regular list.
   *  Server-authoritative (the list endpoint's `pinned` field); toggled
   *  via `PUT /v1/chat/sessions/:id/pin` and kept in sync across tabs by
   *  `Frame::SessionUpdated` patches. */
  pinned: boolean;
  /** Preview text the sidebar row renders — the session's most-recent
   *  user-authored message, truncated server-side. `undefined` for
   *  brand-new sessions with no user turn yet, and for sessions whose
   *  preview fetch failed on the list call. Updated locally on send
   *  and on inbound `Frame::UserEcho` so the row reflects the latest
   *  prompt without a list refetch. */
  last_user_text?: string;
}
