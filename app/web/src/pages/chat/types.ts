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
  /** Auto-generated title used before falling back to `last_user_text`. */
  title?: string;
  /** Bound agent profile id; absent = builtin. Server-authoritative,
   *  set at creation and never changes for the session's lifetime — carried
   *  on the list endpoint, on `SessionPatch.agent_id` (Create/Unhide
   *  broadcasts), and stamped locally on the optimistic new-chat prepend. */
  agent_id?: string;
  /** The recurring cron job this conversation is a *fire* of, when it is one.
   *  Present only on listed cron conversations; absent on user chats. The
   *  sidebar collapses all fires of one job into a single derived **cron
   *  group** keyed by this id (`docs/cron-groups.md`) — and a row that has it
   *  is grouped by it and never by `folder_id`. Server-authoritative and
   *  immutable (the list endpoint's `cron_job_id`); it is read off the
   *  session's trigger, so no patch ever changes it. */
  cron_job_id?: string;
  /** The cron group's label — the job's live title, falling back to the title
   *  snapshotted at fire time once the job is deleted. Absent when the row can
   *  be named from neither source, in which case it stays flat (ungrouped)
   *  rather than being given an invented name. */
  cron_job_title?: string;
  /** Whether this row's cron GROUP is pinned (`PUT /v1/cron/{id}/pin`). The bit
   *  lives on the JOB — the group is a view, and the job is the only object
   *  whose identity matches it — so every fire of the job carries the same
   *  value and the sidebar folds it into the one group header. Distinct from
   *  `pinned`, which is this SESSION's own pin. A tombstone group (job deleted)
   *  is always `false`: there is nothing left to hold the bit. */
  cron_group_pinned?: boolean;
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
