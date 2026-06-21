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
  /** The user-created folder this session is filed under, or `undefined`
   *  for uncategorized. Server-authoritative (the list endpoint's
   *  `folder_id`); changed via `PUT /v1/chat/sessions/:id/folder` and
   *  kept in sync across tabs by `Frame::SessionUpdated` patches carrying
   *  a `folder_id` change. */
  folder_id?: string;
}

/** A user-created chat-list folder. Two-level tree via `parent_id`
 *  (`undefined` = top-level). Server state, seeded from
 *  `GET /v1/chat/folders` and replaced wholesale on every
 *  `Frame::FoldersChanged` snapshot. */
export interface Folder {
  id: string;
  parent_id?: string;
  name: string;
  position: number;
  created_at: string;
}
