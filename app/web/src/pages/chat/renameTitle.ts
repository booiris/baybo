/** Conversation-rename rules, kept out of `SessionSidebar` so they can be
 *  tested without mounting the sidebar and its DndContext. */

/** Mirrors `baybo_model::MAX_SESSION_TITLE_LEN`. The server is the authority
 *  and answers 400 past it; bounding here just keeps the user from typing into
 *  a rejection. Counted in code points to match the server's `chars().count()`
 *  rather than JS's UTF-16 units, so an emoji or CJK title agrees with it. */
export const MAX_SESSION_TITLE_LEN = 80;

/** Truncate to the server's cap, counting code points. */
export function capTitle(text: string): string {
  return [...text].slice(0, MAX_SESSION_TITLE_LEN).join('');
}

/** The draft to open the editor with: whatever the row currently shows.
 *
 *  Truncated, because a cron fire's title is minted server-side without this
 *  bound — seeding it whole would produce a draft the server refuses even if
 *  the user changes nothing. */
export function seedTitleDraft(session: {
  title?: string;
  last_user_text?: string;
}): string {
  return capTitle(session.title ?? session.last_user_text ?? '');
}

/** The title to send, or `null` to send nothing.
 *
 *  Compared against the seed rather than the row's stored title: an untouched
 *  editor must commit nothing, and for an untitled row the seed is its
 *  `last_user_text` preview, so comparing against `title` would turn a stray
 *  blur into a real rename that also suppresses the auto-titler. */
export function titleToCommit(draft: string, seed: string): string | null {
  const title = draft.trim();
  if (title.length === 0) return null;
  return title === seed.trim() ? null : title;
}
