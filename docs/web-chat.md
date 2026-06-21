# Web chat (`web/src/pages/ChatPage.tsx` + `web/src/pages/chat/`)

A feature reference for the Aura **web chat UI** — the `/chat` route of the embedded React
dashboard. It documents *what the chat does*, organised by subsystem. For the dashboard's
build/embed pipeline, OpenAPI→TypeScript codegen, and asset handling see
[`webui.md`](webui.md); for the neo-brutalist visual language and the three-zone chat layout
see [`../web/CLAUDE.md`](../web/CLAUDE.md); for the gateway REST + chat-WebSocket surface the
UI talks to see [`modules/gateway.md`](modules/gateway.md).

The chat lives almost entirely in one large component, `web/src/pages/ChatPage.tsx`, with
focused helpers under `web/src/pages/chat/` (sidebar, folders, the interjection-queue store +
panel, the input-history ring, attachment rendering) and `web/src/components/` (the cron
inbox, the icon rail). Several keyboard affordances (the input-history ring, slash-command
completion) are deliberate ports of the terminal client (`crates/tui/`); those parities are
called out where they apply.

> Inline `~:NNN` line references are approximate navigation hints anchored to the named
> symbol — grep the symbol, not the number.

## Contents

- [Conversation list & sidebar](#conversation-list-sidebar)
- [Conversation folders](#conversation-folders)
- [Composer: send, stop, attachments, model](#composer-send-stop-attachments-model)
- [Slash commands & input history](#slash-commands-input-history)
- [Interjection queue](#interjection-queue)
- [Thread rendering & turn lifecycle](#thread-rendering-turn-lifecycle)
- [Connection, frames & data flow](#connection-frames-data-flow)
- [Cron inbox & navigation shell](#cron-inbox-navigation-shell)

## Conversation list & sidebar

The chat sidebar (`web/src/pages/chat/SessionSidebar.tsx`) is "zone 2" of the three-zone `/chat` layout (zone 1 = the global icon rail, zone 3 = thread + composer). It is a fixed-width (`w-[260px]`) `bg-canvas` column rendered from `ChatPage` (`web/src/pages/ChatPage.tsx`, around line 2192) with the full `SessionSummary[]` list, the active session id, the set of sessions with a pending approval, and a bundle of mutation callbacks. The list itself is a newest-first set of compact single-line rows organised into a two-level folder tree, with a lifted `Pinned` block on top and a trailing `Uncategorized` bucket. Folder mechanics (drag-and-drop filing, the tree, context menus) are documented separately; this section covers the per-conversation row behavior and the list's live-update model.

### New-chat button

A full-width amber `New chat` button (`RiAddLine`) sits in the sidebar header. Clicking it calls `onNewChat` → `handleNewChat` (`ChatPage.tsx` ~2147), which `POST /v1/chat/sessions`, prepends a synthetic row to `sessions` (newest-first), and navigates to `/chat/<new id>`. The prepend is guarded by a `prev.some(s => s.session_id === data.session_id)` check because the server's `Created` broadcast (a `Frame::SessionUpdated` full patch) also reaches this tab and `applySessionPatch` would otherwise add the row a second time — whichever path runs first wins, the second is a no-op. The button is `disabled` while `creating` is true (a request is in flight) and dims via `disabled:opacity-50`. A sibling action, `New chat here` from a folder's context menu, routes through `onNewChatInFolder` and immediately `PUT`s the new session's folder before navigating.

### List ordering: newest-first, with pinned lifted out

Incoming server order is newest-first and the sidebar **never reorders rows in place**. `applySessionPatch` and `applySessionActivity` (`ChatPage.tsx` ~3747 / ~3812) both merge fields onto the existing row by index and explicitly leave position untouched — a live activity bump or reply must not reshuffle the list under the user. A genuinely new session (a patch for an unknown `session_id` carrying `created_at` + `last_active`) is **prepended**, so only creation changes order. Within `SessionSidebar`, the rows are then bucketed (`useMemo`, ~511): every `pinned` session is lifted entirely out of the folder tree into the top `Pinned` block; non-pinned sessions are grouped by `folder_id`; a session whose `folder_id` points at a folder that isn't *reachable* in the rendered tree (dangling parent, cycle, or depth ≥ 2 grandchild) falls back to `Uncategorized` rather than vanishing. Bucketing preserves the incoming newest-first order within each block. The `Pinned` block only renders when at least one session is pinned.

### Active-row highlight

The active row (`session.session_id === activeSessionId`) renders with the coral `bg-selected` background, a solid `border-black` + `shadow-brutal-sm`, and a **bold** title; inactive rows are borderless with a `hover:bg-gray-100` and regular-weight title (`SessionSidebar.tsx` ~133). Each row is a react-router `<Link>` to `/chat/<id>`, so clicking navigates. The whole row is also `useDraggable` (a chat is a plain draggable, deliberately *not* sortable, so dragging it never reflows the list — it only lifts out to drop onto a folder).

### Unread badge

`SessionSummary.unread` is a **local-only** counter; the server never surfaces it. The sidebar derives it from inbound `Frame::SessionActivity` pings via `applySessionActivity`: an activity for a background session bumps `unread + 1`; an activity for the currently-foregrounded session does not bump (the user can see it). The badge is a brand-gold pill showing the count (capped at `99+`) and renders only when `unreadCount > 0 && !active` (`SessionSidebar.tsx` ~118/179). The count is **cleared on navigation into the session**: a `sessionId`-keyed effect in `ChatPage` (~1092) zeroes the entered row's `unread`. `Frame::SessionActivity` is fired by the gateway's dispatch observer regardless of WS subscription state, so background sessions accumulate unread badges even when their transcript isn't being streamed; the `currentSessionIdRef` decides foreground-ness in the frame router (~782). Activity for a session the tab doesn't know about is dropped on the floor (and instead nudges the CronInbox to refetch, since it's probably a cron-spawned session filtered out of the chat list server-side).

### Relative timestamp that swaps to a delete button on hover

Each row's right slot shows a mono relative-age string from the local `relativeAge` helper (`SessionSidebar.tsx` ~67): `just now`, `<n>s/m/h/d ago`, then a `Mon D` locale date past a week. It is fed by `session.last_active`, which `applySessionActivity` keeps current by projecting the activity ping's `at` onto the row (taking the later of old/new) — so the age stays fresh without a list refetch. The timestamp is `group-hover:hidden`; **no space is reserved for the row actions**, so on row hover the timestamp is replaced in place by a pair of icon buttons: a pin/unpin toggle and a hide (delete) button. (The hide button uses `RiDeleteBin6Line`, the trash glyph, but its action is *hide*, not delete — see below.) Both action buttons call `e.preventDefault()/stopPropagation()` and `onPointerDown` stops propagation so clicking them neither follows the `<Link>` nor starts a drag.

### Rename

There is **no inline rename for conversation rows.** The row title is non-editable; it renders `session.last_user_text` — the session's most-recent user-authored message, truncated — or the italic placeholder `New conversation` when no user turn exists yet (`SessionSummary.last_user_text` is `undefined`). The preview is updated locally without a list refetch on this tab's own send and on inbound user echoes via `applySessionUserText` (`ChatPage.tsx` ~3845), which collapses whitespace and truncates to `PREVIEW_MAX_CHARS` (120) to mirror the server's `truncate_preview`. (Inline rename *does* exist, but only for **folders**, via `FolderHeader`'s rename input — that's a folder feature, not a conversation feature.)

### Hide vs. delete, and the HideSessionModal confirmation

"Removing" a conversation from the list is a **hide**, not a destructive delete — consistent with the project invariant that session rows/transcripts are core data and must never be dropped by the runtime. The hide button's tooltip says so explicitly: "Hide from list (server-side row is kept)". Clicking it calls `onHide` → `handleHideSession` (`ChatPage.tsx` ~1897), which sets `hidePrompt` to the id and opens `HideSessionModal` (`ChatPage.tsx` ~2535) — an in-app confirmation replacing the browser's native `confirm`. The modal is titled `Remove conversation`, asks "Remove this conversation from your list?", and offers Cancel / Remove. Backdrop click and the `Escape` key both cancel (while idle); cancel is suppressed while `hideSubmitting`. Confirm (`confirmHideSession` ~2058) issues `DELETE /v1/chat/sessions/{session_id}` — which on the server side only flips the `hidden` flag — then removes the row locally and releases its WS view. If the active session was the one hidden, it navigates to a fallback session (first remaining, else the anchor) or to `/chat`. A server-side failure (e.g. 404) is surfaced inside the still-open dialog (`HTTP <status>` or the error body) and the row stays visible — the hide is server-authoritative. The local DELETE and a sibling tab's hide converge on the same `applySessionPatch` path: a `SessionUpdated` patch with `hidden: true` filters the row out and (in the frame router, ~763) releases the view and redirects if it was foregrounded.

### Pin / unpin

The hover pin button (and the context-menu `Pin`/`Unpin` action) calls `onTogglePin` → `handleTogglePin` (`ChatPage.tsx` ~1906), which is **optimistic**: it flips the local row's `pinned` immediately so the sidebar re-buckets (the row jumps to/from the `Pinned` block) and then `PUT /v1/chat/sessions/{session_id}/pin`. On request failure it reverts the optimistic flip. The server's `SessionPatch` broadcast (`pinned` field on a `Frame::SessionUpdated`) converges every tab. Pinned rows also carry a **persistent fill-pin glyph** (`RiPushpin2Fill`) before the title so the state reads at a glance even without hovering; that glyph is `group-hover:hidden` so it yields to the interactive toggle on hover. Pin state is independent of folder assignment — a pinned row stays in the `Pinned` block regardless of its `folder_id`, and assigning a folder to a pinned row (`handleAssignFolder`) does *not* auto-unpin; the assignment simply takes effect when it's later unpinned.

### Search / filter

There is **no search or filter box in the sidebar** — the component renders no text input over the conversation list, and the list is never filtered by a query. (The only inputs in the sidebar are the inline folder-name fields.) Discovery is by scroll, the `Pinned` lift, and folder organisation only.

### Other row affordances

Two more badges can appear in a row's right slot, left of the timestamp:

- **Queued-message badge** — a stacked-glyph (`RiStackLine`) pill showing the count of parked interjection-queue messages for that session, read from the shared `useQueueCounts()` store (independent of WS subscription). Unlike the unread badge, it is shown for **all** rows including the active one. Capped at `99+`.
- **Approval-pending dot** — a small `bg-warn` (white on the active row) dot, titled "Approval pending", rendered when the session id is in `pendingIds` (`pendingApprovalIds`, derived in `ChatPage` from each view's `pendingApproval`).

### Live updates: frames and invariants

The list reacts to three WS frame kinds (routed in `ChatPage`'s WS effect, ~761):

- `Frame::SessionUpdated { session_id, patch }` → `applySessionPatch`. `patch.hidden === true` removes the row; an unknown `session_id` with `created_at` + `last_active` constructs and prepends a new row (a sparse `last_active`-only patch for an unknown id is dropped — `Created` arrives separately); otherwise present fields (`created_at`, `last_active`, `pinned`, three-state `folder_id` as absent/`uncategorized`/`{set:{id}}`) merge in place, and the function returns `prev` unchanged when nothing actually differs (referential-stability guard against needless re-renders).
- `Frame::SessionActivity { session_id, at }` → `applySessionActivity` (drives `last_active` freshness + the unread bump, as above).
- User-echo / replay paths → `applySessionUserText` (drives the preview title).

Invariants worth calling out, since they're easy to misread: (1) rows are **never repositioned** by a live update — only creation prepends, so the newest-first order is stable under concurrent replies; (2) the unread counter is **client-derived and per-tab**, not server state, and is always 0 on the foregrounded row; (3) the timestamp/age is recomputed from `last_active` on each render, not persisted; (4) the trash icon is hide-semantics, never a real delete.

## Conversation folders

The chat sidebar (`web/src/pages/chat/SessionSidebar.tsx`) organizes conversations into a user-created, two-level folder tree, rendered above a trailing **Uncategorized** bucket and below a lifted **Pinned** block. Folders are server state; per-folder collapse is the only client-local bit. The store lives in `web/src/pages/chat/folderStore.tsx`, context menus in `web/src/pages/chat/FolderContextMenu.tsx`, and all REST handlers in `web/src/pages/ChatPage.tsx` (`handleAssignFolder`, `handleCreateFolder`, `handleRenameFolder`, `handleMoveFolder`, `handleReorderFolders`, `handleDeleteFolder`, `handleNewChatInFolder`).

### Two-level folder tree

A `Folder` (`web/src/pages/chat/types.ts`) is `{ id, parent_id?, name, position, created_at }`; `parent_id` absent means top-level. Depth is capped at two levels — `MAX_DEPTH = 2` in `SessionSidebar.tsx`. The sidebar renders `topLevelFolders` (children of the `null` parent key) and, under each, its direct subfolders, recursively via `renderFolderSubtree(folder, depth)`. The recursion stops descending past depth 1: `subFolders = depth + 1 < MAX_DEPTH ? childFolders.get(folder.id) : []`, so a depth-2 grandchild folder is never walked.

`childFolders` is a `Map<parent_id|null, Folder[]>` with each sibling group **sorted by `position` ascending** — this `position` ordering is what folder reordering and nesting mutate server-side. Within a folder, subfolders render first, then the folder's direct chats (`directChats`).

Chats are bucketed by **reachability, not mere existence** (`reachableFolderIds`): only top-level folders plus folders whose parent is itself top-level are reachable. A chat whose `folder_id` points at a dangling parent, a cycle, or a depth-≥2 grandchild — folders that exist in the list but render nowhere — falls back to Uncategorized instead of vanishing. A chat with a `folder_id` that no longer exists at all does the same.

Each `FolderHeader` shows a collapse caret, a folder glyph (`RiFolder3Line` closed / `RiFolderOpenLine` open), and the name (or an inline rename input).

### Uncategorized bucket

The trailing **Uncategorized** block (`UncategorizedDropZone`) holds every non-pinned chat not filed under a reachable folder. It is always rendered, even when empty, where it shows the hint "Drop a chat here to remove its folder." It doubles as a drop target (dnd id `drop:uncategorized`): dropping a chat clears its folder, and dropping a folder promotes that folder to top-level.

### Create / rename / delete (delete dissolves, never deletes chats)

- **Create** — the "Folders" section label carries a `RiFolderAddLine` button that opens an inline `NewFolderRow` at top level (`creatingIn = null`); a folder's context-menu "New subfolder" opens one under that parent (`creatingIn = <parentId>`, only offered for top-level folders, gated by `canAddSubfolder = folder.parent_id == null` to keep depth ≤ 2). Enter commits a trimmed non-empty name, Escape cancels, blur commits-or-cancels. Commit calls `onCreateFolder(name, parentId?)` → `POST /v1/chat/folders` with `{ name, parent_id }`.
- **Rename** — context-menu "Rename" (or starting a rename) swaps the header label for an inline input seeded with the current name. `commitRename` fires `onRenameFolder` → `PATCH /v1/chat/folders/{folder_id}` only when the trimmed name is non-empty **and** actually changed. Note: the PATCH is fired from the handler body, not inside a `setState` updater, specifically because React StrictMode double-invokes updaters in dev and would send the request twice.
- **Delete** — the folder context menu's "Delete folder" requires an inline **"Delete?" Yes/No** confirm (state local to `FolderContextMenu`, not a native `confirm`). Confirm calls `onDeleteFolder` → `DELETE /v1/chat/folders/{folder_id}`. **Deleting a folder dissolves it — it never deletes the chats inside it.** Client-side this happens by convergence: the server broadcasts a `Frame::FoldersChanged` snapshot (folder gone) plus `SessionPatch.folder_id = 'uncategorized'` for each affected chat, and `applySessionPatch` (`ChatPage.tsx`) clears those rows' `folder_id` so they reappear under Uncategorized.

All folder CRUD handlers are **fire-and-converge**: they do not optimistically mutate the store-owned folder list; they wait for the server's `Frame::FoldersChanged` snapshot. On error they only `console.warn`.

### Drag and drop (chats and folders)

Drag-drop uses `@dnd-kit/core` + `@dnd-kit/sortable`. Folders and chats share one `DndContext` with `pointerWithin` collision detection; dnd ids are namespaced (`chat:<id>` / `folder:<id>`) and parsed back on drag end via `parseDragId`. A `DragOverlay` renders a floating preview (`ChatDragPreview` for chats, an inline folder chip for folders); the dragged source dims in place (`opacity 0.4`). The pointer sensor has a 4px activation distance so a click-to-open doesn't register as a drag.

Crucially, **chats are plain `useDraggable`, not sortable** — they are deliberately kept out of `SortableContext` (`allSortableIds` contains only folder ids). Chat list order is server-driven (newest-first within each bucket) and is **not** user-sortable; dragging a chat only lifts it out to re-file it, and never reflows the list. Folders use `useSortable` (which registers the same id as both the sortable and the drop target via one `setNodeRef`/`isOver`, avoiding a double-registration race).

`onDragEnd` resolves by source kind:

- **Chat → Uncategorized drop zone** → `onAssignFolder(chatId, null)`.
- **Chat → folder header** → `onAssignFolder(chatId, folderId)`.
- **Chat → another chat** → ignored (order is server-driven).
- **Folder → Uncategorized drop zone** → promote to top-level (`onMoveFolder(id, null)`), no-op if already top-level.
- **Folder → folder, same parent** → reorder within the sibling group via `arrayMove` → `onReorderFolders(parentId, orderedIds)`.
- **Folder → folder, different parent** → nest under the target (`onMoveFolder(id, targetId)`), with client-side guards that reject the move (server is the final arbiter) if it would exceed depth 2 (target is itself nested), if the dragged folder already has children, or if dropping onto itself.

`onAssignFolder` → `PUT /v1/chat/sessions/{session_id}/folder`, `onMoveFolder` → `POST /v1/chat/folders/{folder_id}/move`, `onReorderFolders` → `POST /v1/chat/folders/reorder`. Folder moves/reorders are fire-and-converge; a chat's folder assignment is **optimistic** (the row's `folder_id` flips locally immediately, reverting on PUT failure).

### Context menus

Right-clicking a folder header opens `FolderContextMenu` (Rename / New chat here / New subfolder [top-level only] / Delete folder); right-clicking a chat row opens `ChatContextMenu` (Move to folder ▸ submenu / Pin·Unpin / Hide). Both are rendered via a `document.body` `createPortal` so they escape the sidebar's `overflow-auto` clip and position at the raw cursor point (clamped to the viewport), and both dismiss on outside-click or Escape. The chat menu's "Move to folder" submenu lists every folder plus an explicit **Uncategorized** entry (`onMoveToFolder(null)`) and flips to the left if it would clip the right edge. "New chat here" calls `onNewChatInFolder` → `handleNewChatInFolder`, which creates a session (`POST /v1/chat/sessions`), optimistically inserts the row with that `folder_id`, files it (`PUT …/folder`), and navigates to it.

### Pinned vs. folder interaction

Pinned chats are **lifted entirely out of the folder tree** into the Pinned block at the top — a chat is filed under at most one display location, and pin wins. A row's `folder_id` is retained while pinned (assigning a folder to a pinned row does **not** auto-unpin; the assignment just takes visible effect once the row is unpinned). Bucketing reflects this: `s.pinned` short-circuits into `pinnedRows` before any folder check. Auto-expand of the active chat's folder path is suppressed for a pinned active chat (`activeFolderId = activeSession?.pinned ? undefined : activeSession?.folder_id`).

### Persistence and convergence model

The folder list is **server state**, mirrored on a flat `sessions.folder_id` column per chat (analogous to the `pinned`/`hidden` flat columns), with the folder rows themselves in their own table. The web folder list is seeded on bootstrap from `GET /v1/chat/folders` and **replaced wholesale** on every `Frame::FoldersChanged` WS snapshot — folders are few, so there is no patch-merge; both paths call `folderStore.replaceFolders` (`ChatPage.tsx`). `WireFolder` mirrors the Rust `FolderView` (`web/src/api/chatWs.ts`).

Per-chat folder assignment converges over `Frame::SessionUpdated` patches. `SessionPatch.folder_id` is the three-state `FolderChange`: **absent** = no change, `'uncategorized'` = clear, `{ set: { id } }` = file under that folder. `applySessionPatch` resolves it to the row's `folder_id` (`undefined` for the first two). This is the same channel folder-delete uses to dissolve chats to Uncategorized.

The **only client-local** folder state is per-folder collapse: `folderStore.tsx` persists the collapsed-id set under the single `localStorage` key `aura.folders.collapsed` (stored as a JSON array; the key is removed when the set is empty). It is synced across tabs via the `storage` event. `collapsed` lives in React state for reactivity; an internal `collapsedRef` is updated synchronously before `setState` so two collapse mutations in one tick compose instead of clobbering through a stale ref. The store exposes an imperative `FolderApi` (`replaceFolders`, `toggleCollapse`, `setCollapsed`, `ensureExpanded`) consumed via `useFolderStore()` (non-reactive, captured once by the WS closure) and a reactive `useFolders()` (`{ folders, collapsed }`) for the sidebar. `ensureExpanded(path)` reveals the active chat's folder path on load (no-op when none on the path are collapsed). The `<FolderProvider>` wraps the app in `web/src/main.tsx`.

Gotchas / invariants: a stale or unreachable `folder_id` never hides a chat (it falls to Uncategorized). Chats are never user-sortable. Folder CRUD is fire-and-converge (no optimistic folder-list mutation); only chat folder *assignment* and the "new chat here" row insert are optimistic. Depth-2 nesting and self/child nesting are guarded client-side but the server is the final arbiter.

## Composer: send, stop, attachments, model

The floating composer pill at the bottom of the chat thread (`web/src/pages/ChatPage.tsx`, the `<form onSubmit={handleSend}>` block ~line 2322) carries the textarea, an attach button, an optional per-session model picker, and a send/stop button. It sits at `max-w-4xl` centered on the reading band; a page-colour gradient behind it fades scrolling bubbles out as they slide under the pill. Slash-command autocomplete and input-ring history are covered in a separate section.

### Send / stop button matrix

The footer renders exactly one of two buttons, chosen by `busy && !hasContent` (~line 2479):

- **Stop button** (red circle, `RiStopFill`) — shown only when `busy && !hasContent`: a turn is in flight *and* the composer has no draft/ready attachment. `onClick={handleStop}` issues `/stop`. Disabled while disconnected.
- **Send button** (amber circle, `RiSendPlane2Line`, `type="submit"`) — shown otherwise. Disabled when there is no session, the WS is not `connected`, any attachment is still `uploading`, or `!hasContent`. Its tooltip flips between "Send (Enter)" and "Queue message (Enter)" depending on `busy || queue.pauseReason !== null`.

`busy` is `currentView.awaitingReply || (currentView.turn?.active ?? false)` (~line 334) — true optimistically between send and the first response, and authoritatively per the server `TurnState`. `hasContent` is `composer.trim().length > 0 || attachments.some((a) => a.status === 'ready')` (~line 1468).

Key invariant: because the stop button only appears when `!hasContent`, typing a draft *while a turn runs* swaps the red stop button back to the amber send button — so a mid-turn submit parks/queues the draft instead of cancelling the turn (cancel is then only reachable by typing `/stop` or clearing the draft). The two branches use distinct React `key`s (`composer-stop` / `composer-send`) and `handleStop` calls `e.preventDefault()`: clicking stop flips `busy` false synchronously mid-click, which would otherwise re-type the same DOM node into the submit button and let the browser run its default submit on it — sending the draft. The distinct keys plus the `preventDefault` both block that. The stop button is `type="button"`; only the send button is `type="submit"`.

### What a submit decides (send vs. queue)

`handleSend` (~line 1471) is the single submit handler for both Enter and the send button. It does **not** early-out on `busy`. After bailing if any attachment is still uploading, it builds the `WireAttachment[]` from `ready` attachments and calls the pure `decideComposerAction({ hasContent, isStop, busy, paused })` (~line 267):

- `!hasContent` → `noop`.
- `/stop` typed → `stop` (bypasses busy/paused), routed to `sendText('/stop')`.
- idle and not paused → `direct` (`sendText(composer, wire)` — sends immediately; the turn's completion auto-drains the queue, so a non-empty queue never stalls a direct send).
- busy **or** paused (after a `/stop`/error) → `park` (`queue.enqueue(...)` into the interjection queue).

`/stop` detection is `isStopCommand` (~line 3483): trimmed text starting with `/` whose first token (split on whitespace or `@`) lowercases to `stop`. The submitted line is recorded in the input ring via `inputHistory.commit` (send, park, or a typed `/stop` all count). After dispatch the composer is cleared, every attachment `previewUrl` is `URL.revokeObjectURL`'d, `attachments` is reset to `[]`, and the slash-hint popup is closed.

`sendText` (~line 1394) routes non-stop sends through `sendToSession`, which appends an optimistic `pending: true` user row, sets `awaitingReply: true`, bumps the per-session turn token, and emits a WS message frame keyed by a fresh `clientMsgId` (same UUID dedups the frame and reconciles the optimistic row against the inbound echo). `/stop` is special-cased entirely client-side: it collapses the live work block to "Cancelled", keeps any partial reply as its own bubble, marks the session stopped (`stoppedSessionsRef`), pauses the interjection queue if items are parked, and sends the `/stop` frame — the server's later `TurnState`/notice frames reconcile idempotently. Both paths require `status.state === 'connected'`; a disconnected `sendToSession` returns false and leaves the item queued.

### Enter to send, Shift+Enter newline, textarea auto-grow

`handleComposerKey` (~line 1741): a bare **Enter** (no Shift) calls `e.preventDefault()` and, if the trimmed draft is non-empty or any attachment is `ready`, calls `form.requestSubmit()` — firing `handleSend`, which decides send vs. park. **Shift+Enter** falls through to the textarea default and inserts a newline. Tab/Arrow keys feed slash-completion and input-history (separate section); Tab never moves focus out of the composer.

The textarea is `rows={1}` and auto-grows in a `useLayoutEffect` keyed on `composer` (~line 1222): it resets `height` to `auto`, then sets it to `min(scrollHeight, 200)` px — single-line when idle, growing to a 200px cap for multi-paragraph drafts, then internal scroll. The placeholder reads `Message Aura…  (Shift+Enter for newline)` when connected, else `Waiting for connection…`.

### Attachments: upload, previews, removal

The attach button (`RiAttachmentLine`, ~line 2460) clicks a hidden `<input type="file" multiple>` (`fileInputRef`). It is disabled when there is no channel token or the WS is not connected.

`handleFilePick` (~line 1692) iterates the picked `File`s, calls `uploadAttachment` per file, then resets `e.target.value = ''` so re-picking the same file still fires `change`.

`uploadAttachment` (~line 1652) immediately pushes a `PendingAttachment` (`status: 'uploading'`) with a fresh `localId` (`crypto.randomUUID()`). For `image/*` mimes it sets `previewUrl = URL.createObjectURL(file)` for an instant local thumbnail (no upload round-trip). It then `POST`s the raw file body to `${baseUrl}/v1/blobs` with headers `x-aura-channel-token: <channelToken>` and `content-type: <mime>` (the web operator token resolves to `AuthedClient::Web`, bypassing pairing). On success it stores the returned content-addressed `blob_id` and flips `status: 'ready'`; on any failure `status: 'error'`. Mime defaults to `application/octet-stream` when the file reports none.

`PendingAttachment` (~line 239) fields: `localId`, `filename`, `mime`, `size`, `status` (`'uploading' | 'ready' | 'error'`), `blobId?` (filled on upload), `previewUrl?` (images only). `attachmentKind(mime)` (~line 251) maps the mime to the wire `kind`: `image/*` → `image`, `audio/*` → `audio`, else `file`. On send, `handleSend` keeps only `ready` attachments with a `blobId` and builds `WireAttachment { kind, blob_id, mime_type, size, filename }` (shape in `web/src/api/chatWs.ts` ~line 20).

Composer chip rendering (~line 2379): attachments with a `previewUrl` render as a 14×14 (`h-14 w-14`) `object-cover` image thumbnail (dimmed at 40% on `error`, with a spinner overlay while `uploading`); all others render as a named chip whose leading icon is a spinner (`uploading`), an X (`error`, error-tinted), or `RiAttachmentLine` (`ready`). Each carries a small remove button. `removeAttachment` (~line 1704) revokes the `previewUrl` if present and filters the item out by `localId`.

Note: in-thread attachment thumbnails are a *separate* concern from composer previews. Sent/received image attachments render via `AttachmentImage` (`web/src/pages/chat/AttachmentImage.tsx`), which fetches `GET /v1/blobs/<blobId>` with the `x-aura-channel-token` header (an `<img>` tag can't send the auth header), turns the blob into an object URL, and shows a spinner while loading / a named placeholder chip on fetch failure. It re-fetches when `channelToken` lands (e.g. a queued attachment whose preview rebuilds after reload) and revokes the object URL on unmount.

### Per-session model picker

When a session is open and `models.length > 1`, the footer renders `ModelPicker` (~line 2471, component ~line 4635) just left of the send button. The model list and the global default name come from `GET /v1/llm/models`, fetched once on mount into `models` / `defaultModelName` (~line 601). The picker is hidden entirely when one or zero switchable models exist.

The button label shows the active pin: `current` (the session's `last_llm`) if set, else `Default · <defaultName>` (or just `Default`). The dropdown (opens upward, `bottom-full`) lists a "Default (default-llm)" row plus one row per model showing `name` over `provider · model`, with a brand check on the selected row. It dismisses on outside `mousedown` or Escape.

`handleSelectModel` (~line 2097) `PUT`s `/v1/chat/sessions/{session_id}/model` with body `{ llm: name }` (`name === null` clears the pin back to `default-llm`). The PUT is authoritative: the response's `last_llm` echo drives the local update (`views[sessionId].model = data.last_llm ?? null`), and a live actor, if any, is re-pinned server-side to take effect on the session's **next** turn. A successful switch is silent; only a failure appends an error notice to the transcript. `ModelPicker.pick` short-circuits re-selecting the current pin (no round-trip) and shows a spinner on the trigger while the `onSelect` promise is in flight.

The pinned model survives reloads because it is a flat session column: `currentView.model` is seeded from the history-load response's `last_llm` (~line 1029) and the picker reads `current={currentView.model}`. `ModelOption` (~line 154) is `{ name, provider, model, isDefault }`.

## Slash commands & input history

The chat composer (`web/src/pages/ChatPage.tsx`) carries two keyboard affordances ported from the TUI (`crates/tui/src/app.rs`): a `/`-prefixed slash-command autocomplete popup and a shell-style Up/Down input-history ring. Both are wired into the single `<textarea>` composer via `handleComposerKey` (`onKeyDown`) and `handleComposerChange` (`onChange`). The composer draft is tab-global — one `composer` string shared across every conversation, never reset on session switch — so the history ring is global too.

### Slash-command autocomplete popup

Typing a `/` as the first character of the draft opens a popup floating directly above the input box listing the matching slash commands; each row shows `/name` in bold plus its description. User-facing behavior:

- **Prefix filter.** The popup shows every command whose name starts (case-insensitively) with the token typed after the `/`, up to the first whitespace. An empty token (just `/`) lists all commands. Computed in `filteredSlash` (~`web/src/pages/ChatPage.tsx:1715`): `composer.slice(1).split(/\s/)[0]` lower-cased, then `slashCommands.filter(s => query.length === 0 || s.command.toLowerCase().startsWith(query))`. Mirrors the TUI's `completion_candidates`.
- **Highlight + Up/Down wrap nav.** One row is highlighted (`selectedSlash` state). While the popup is open, `ArrowUp`/`ArrowDown` move the highlight and **wrap** at the ends (`i <= 0 ? len-1 : i-1` and `(i+1) % len`), with `preventDefault` so the arrows do not move the caret and do not fall through to the history ring (the popup nav is checked before the ring). Hovering a row with the mouse also sets the highlight (`onMouseEnter`).
- **Tab / click to accept.** `Tab` (unshifted) accepts the highlighted candidate via `completeSlash(Math.min(selectedSlash, filteredSlash.length - 1))`; clicking a row accepts that row. Acceptance runs `applySlashCompletion` (~`:282`), which replaces the leading `/command` token (everything up to the first whitespace) with `/name ` (trailing space included) **preserving any trailing args** after the command, and lands the caret at offset `name.length + 2` — just after the inserted `/name ` (the `+2` covers the leading `/` and the trailing space). The popup then closes and `selectedSlash` resets to 0. Port of the TUI's `completion_accept`.
- **Enter still SENDS, never accepts.** Unshifted `Enter` always submits the form (`form.requestSubmit()`), even with the popup open and a candidate highlighted — it does not accept the completion. So a fully-typed `/stop` or `/clear` sends on Enter without a detour through Tab. (`Shift+Enter` inserts a newline.)
- **Tab never moves focus.** `Tab` is `preventDefault`-ed unconditionally in the composer, so it never tab-jumps focus to the footer buttons (attach/model/send); with no popup open it is simply swallowed.
- **Caret-aware (closes on args).** The popup is open only while the caret is still on the `/command` token; the moment the caret moves past the first whitespace into the arguments it closes (pure `caretOnSlashToken`, ~`:296`). So while editing args, `Tab` stops re-completing and `↑`/`↓` revert to the history ring — the web port of the TUI's `cursor > prefix_end` guard. The check re-runs on every edit and caret move (`onChange` + `onSelect`), so moving the caret back onto the token reopens it.

Implementation notes:
- Candidate list source: `slashCommands` state, fetched once on mount from the session-bootstrap manifest (`setSlashCommands(manifest?.items ?? [])`, ~`:620`), each item `{ command, description }`.
- `showSlashHints` (recomputed by `refreshSlashHints` on both `onChange` and `onSelect` as `slashCommands.length > 0 && caretOnSlashToken(value, caret)`) gates `filteredSlash`; the popup JSX renders only when `filteredSlash.length > 0` (~`:2351`). Editing always resets `selectedSlash` to 0 (a refiltered list invalidates the old index). `setShowSlashHints` is a no-op when the boolean is unchanged, so the `onSelect` caret tracking only re-renders on an actual open/close transition.
- Popup buttons use `onMouseDown={e => e.preventDefault()}` to keep focus in the textarea so the post-completion caret placement lands the user back in the box ready to type args.
- `completeSlash` short-circuits if the replacement equals the current text, to avoid stranding `pendingCaret` on a bailed-out render (see caret note below).
- The popup is anchored to the composer pill (`absolute bottom-full`), not the form, so the interjection queue panel sitting above the pill cannot push it up.

### Input-history ring (Up/Down recall)

With an empty (or already-recalled) composer, pressing Up walks back through previously submitted messages and Down walks forward toward the empty draft — a shell-style ring. Logic lives in pure helpers in `web/src/pages/chat/inputHistory.ts` (a faithful port of the TUI's `remember` / `history_prev` / `history_next`), wrapped by the `useInputHistory()` hook; pinned by `web/src/pages/chat/inputHistory.test.ts`.

User-facing behavior:
- **Up recalls older, Down recalls newer.** From an empty composer, Up jumps to the newest submitted entry, then each further Up steps to older entries and **clamps** at the oldest (`Math.max(0, cursor - 1)`). Down steps back toward newer entries; stepping past the newest entry drops back to the **empty draft** (`text: ''`, `cursor: null`).
- **Enter-only-from-empty.** History is entered only from an empty composer (or while already navigating). From a **non-empty fresh draft**, Up is a no-op (`cursor === null && !composerEmpty` returns `text: null`) so an in-progress draft is never clobbered. Once navigating (cursor set), further Up keeps walking regardless of the recalled text's emptiness (`composerEmpty` is irrelevant mid-navigation).
- **Caret-to-end on recall.** A recall replaces the whole composer and parks the caret at the end of the recalled text (`pendingCaret.current = text.length`).
- **Caret-move / edit exits history mode.** Any composer edit (`handleComposerChange`) calls `inputHistory.reset()`, and any non-recall caret move — `ArrowLeft`/`ArrowRight`/`Home`/`End`, or a modified arrow — also resets the cursor, matching the TUI's reset-on-every-non-history-action so a later Down cannot jump to a stale entry. A mouse click in the textarea (`onMouseDown`) resets too.
- **Multi-line drafts keep native cursor movement.** Bare Up only walks history when the caret is on the first visual line (no `\n` before the caret); bare Down only when the caret is on the last line (no `\n` after it). Otherwise the arrow falls through to the browser's native cursor movement. A no-op recall (empty ring, or non-empty fresh draft) also falls through to the default.
- **IME guard.** While an IME composition is active (`e.nativeEvent.isComposing`), the handler returns early — ahead of Enter, Tab, slash-nav, and the ring — so Up/Down navigate the IME candidate list and the Enter that commits a CJK candidate does not submit the half-composed draft.
- **Commit on submit.** Every submit records the line — direct send, queue park, or a typed `/stop` all count (`inputHistory.commit(composer)` in `handleSend`, ~`:1498`). `commit` trims, drops empties, and resets navigation.

Implementation notes:
- **One global localStorage ring.** Key `aura.inputHistory` (`HISTORY_KEY`), a JSON string array, loaded once lazily and persisted on change. Up recalls the last thing submitted in this browser regardless of which conversation is open — a single ring like the TUI's, surviving reload the way the TUI ring survives a restart.
- **Cap 500, consecutive-dedup.** `HISTORY_CAP = 500`; `appendHistory` trims the line, **rejects empty/whitespace-only**, drops a **consecutive** duplicate of the newest entry (a non-adjacent repeat is kept), and slices to the most recent 500. It returns the same array reference when nothing changed, so the caller skips the persist write. Persistence is best-effort (a full/blocked store silently degrades to session-only).
- **No re-renders from navigation.** `useInputHistory` keeps both `entries` and the navigation `cursor` in refs, so walking history and recording sends never re-render `ChatPage` — only the caller's `setComposer` does. `historyPrev`/`historyNext` return a `HistoryNav { cursor, text }` where `text === null` means leave the composer untouched and any string (including `''`) is the new value.
- **Caret placement.** Programmatic composer replaces (history recall and slash completion alike) set `pendingCaret.current`; a `useLayoutEffect` keyed on `composer` then focuses the textarea and `setSelectionRange`s to that offset (clamped to the current value length). The recall helper skips the `setComposer` when the recalled text already equals the composer (e.g. re-pressing Up at the oldest entry) so a bailed-out render cannot strand `pendingCaret`.

## Interjection queue

A per-session queue of operator messages parked above the composer while a turn is in flight (or while the session is paused after a `/stop` / error). Parked messages fire one-per-turn-completion (auto-fire), on demand (per-row send), or in bulk (the banner). The feature is entirely client-side except for the batch wire frame; queues persist to `localStorage` so a reload keeps them. Sources: `web/src/pages/chat/queueStore.tsx` (state + persistence), `web/src/pages/chat/QueuePanel.tsx` (panel UI), `web/src/pages/ChatPage.tsx` (composer routing, auto-fire pipeline, deferred-thread rendering), `web/src/pages/chat/SessionSidebar.tsx` (badge).

### Park vs direct-send vs defer-to-thread

A composer submit routes through `decideComposerAction({ hasContent, isStop, busy, paused })` (`ChatPage.tsx`), which returns one of `'noop' | 'stop' | 'direct' | 'park'`:

- **`direct`** — only when `!busy && !paused`. The message starts a turn immediately via `sendText`. The rule is intentionally independent of how many items are already queued: an idle, unpaused send always goes direct, so a non-empty queue can never stall the composer (the started turn's completion auto-drains the queue).
- **`park`** — while a turn is in flight (`busy`) **or** the pipeline is paused after a `/stop`/error (`pauseReason !== null`). The message is appended to the session's parked `items` (`queue.enqueue`) and shown in the panel; it is *not* sent yet.
- **`stop`** — a typed `/stop` (per `isStopCommand`) always bypasses, even busy/paused.
- **`noop`** — empty draft with no ready attachment.

`busy` is `awaitingReply || turn.active`. The send button's tooltip flips to `"Queue message (Enter)"` whenever `busy || pauseReason !== null`, otherwise `"Send (Enter)"` (`ChatPage.tsx` ~2502). Enter submits whether idle or busy — `handleSend` decides; there is no busy early-out. Attachments still uploading block the submit entirely so an in-flight file is never silently dropped.

**Defer-to-thread** is a third disposition reached only via the per-row send button (`fireQueuedItem`), not the composer. If the agent is streaming its *final reply* (`streamingAnswerRef` has the session — set on `answer_delta`, cleared at turn start/end, on any non-answer progress frame, and on `/stop`), there are no tool boundaries left to interject at, so firing the item now would race the turn's end (double-fire) or split the streaming answer into two bubbles. Instead the item is moved to the session's `deferred` list (`queue.deferItem`): it leaves the panel and renders as a dimmed `pending` user bubble pinned *below* the agent's output (`ChatPage.tsx` ~2278, `queue.deferred.map`), never woven into the transcript array. It dispatches when the turn completes (see auto-fire) or, if the turn ends without a normal reply, is moved back to the parked queue.

### Auto-fire on turn completion (turn-dedup)

`drainQueueOnFrame` (`ChatPage.tsx`), kept current in `queueFrameRef` and invoked from the WS `onFrame` closure after every frame, drains the queue on a **live normal completion**: an assistant `message` frame (`role !== 'user'`) for a session whose turn this page-load armed.

Turn-dedup uses two ref-backed maps, `turnTokenRef` and `firedForTurnRef`, both keyed by session id:

- The token is bumped only on a **live** turn start — a user send into the session (`sendToSession` / `sendBatchToSession`) or a live `turn_state{active:true}` frame — and `firedForTurnRef` is cleared on each bump.
- A session with **no token entry** has had no live turn this page-load, so reload catch-up replays (which arrive before the `turn_state` snapshot) never spuriously drain the queue.
- On a qualifying `message` frame the drain fires **at most once per token** (`firedForTurnRef.get(sid) !== token` gate, then `.set(sid, token)`), then sends the top parked item and removes it (`store.removeItem`). Parked items thus fire **one-per-completion**.

Guards: a `/stop`'d session (`stoppedSessionsRef`) salvages its partial reply as an assistant `message` — that is **not** a normal completion and must never drain (early `return`). A paused queue (`pauseReason !== null`) never auto-fires. If the WS rejected the send (`sendToSession` returned false, disconnected) the fired mark is rolled back so a later frame can retry.

Deferred items take precedence over parked items in the same drain: when `deferred.length > 0`, *all* deferred messages dispatch together (see batch send) and the parked items keep waiting; the parked `items[0]` only fires when `deferred` is empty. Both share the single token gate, so a completion fires exactly one of the two paths.

### Manual per-item fire

Each queued row has a send-icon button (`QueuedRow` in `QueuePanel.tsx` → `onFire` → `fireQueuedItem`). It jumps the queue: mid-tool-work it lands as an interjection (a new turn-extending user send via `sendToSession(..., { foreground: true })` with the open work block relabelled `Worked Xs` but kept expanded — see `settleActiveWork`); idle it starts a turn; mid-final-reply it defers to the thread instead (above). The item is removed from the panel only after the WS accepts it. `fireQueuedItem` preserves any `pauseReason` (unlike the banner's resume, which clears it).

### Reorder (drag), edit, delete

The panel (`QueuePanel.tsx`) renders parked `items` as a FIFO list — newest at the bottom, top item ("next") fires first — inside a `@dnd-kit` `DndContext`/`SortableContext` (vertical-axis + parent-element restricted; `PointerSensor` 4px activation, `KeyboardSensor`). Drag the 6-dot handle to reorder; `handleDragEnd` calls `reorder(arrayMove(...).map(i => i.id))`, which `queueStore.reorder` applies by reindexing `items` against the given id order (ids not found are dropped). Drag is disabled on a row while it is being edited.

Each row also has:
- **Edit** (pencil) — opens an inline `textarea` (Enter saves, Shift+Enter newline, Esc cancels). `editItem` updates only the text; a save is refused if it would leave the row fully empty (blank text *and* no attachments), so a row never collapses to nothing.
- **Delete** (trash) — `removeItem` drops the row from `items`.

Row action buttons `stopPropagation` on pointer-down so a click can't start a drag. A "scroll for more" hint appears while the (max-height `56`) list overflows and isn't scrolled to the bottom.

### Pause banner after /stop or error, and resume

When a turn the queue was waiting on is cancelled or fails, the pipeline pauses and a pinned banner (`CancelledBanner` in `QueuePanel.tsx`) appears above the list. `pauseReason` is set to:

- **`'cancelled'`** — on a typed/clicked `/stop` (`sendText` sets it synchronously *before any frame* when `items` is non-empty, after `restoreDeferred` so deferred items are folded back first), and on a broadcast `/stop` cancellation notice (`isStopCancellationNotice`, the `STOP_CANCELLED_NOTICE_MARKER` substring) in `drainQueueOnFrame`.
- **`'error'`** — on a terminal (`!transient`) `notice` with `level === 'error'`.

The banner copy is `"Turn cancelled — send the remaining queued messages?"` / `"Turn failed — …?"` with a coral/`warn` vs `err` tone. Its **Send remaining** button calls `resumeQueue`: it clears the pause and fires the top item now (`sendToSession(..., { foreground: true })`, then `popTop`), after which the one-per-completion pipeline resumes for the rest. `clearPause` then `popTop` compose in one React tick because the store updates its ref synchronously (`apply` writes `queuesRef.current` before `setQueues`).

Invariant: a `pauseReason` is meaningless once `items` is empty (nothing to resume, and a stale pause would silently force-park the next message and gate off auto-fire). `normalize` collapses `pauseReason → null` on every mutation whenever `items.length === 0`, so no drain path can leave a session stuck-paused with zero items.

### Sidebar queue badge

`SessionSidebar.tsx` shows a per-session count badge (stack icon + number, `99+` cap) on **every** row including the active one (`queueCount > 0`). The count comes from `useQueueCounts()` (`queueStore.tsx`), a reactive `Map<sessionId, count>` where `count = items.length + deferred.length` — so both parked and still-pending deferred messages contribute. It reads the shared queue store directly, independent of any WS subscription.

### Multi-session queues

Queues are keyed by session id and live in one shared store (`QueueProvider`), so messages can be parked/fired into **any** tracked session, not only the one on screen. `sendToSession` / `sendBatchToSession` and the whole auto-fire pipeline are session-agnostic; `drainQueueOnFrame` keys every decision off `frame.session_id`. A background session's turn completing drains *its* queue. `useSessionQueue(sessionId)` is the reactive per-session view (panel, composer decision, badge); `useQueueStore()` is the imperative handle captured in `queueStoreRef` for the captured-once WS `onFrame` closure (live ref reads, never re-renders on queue change).

### Atomic batch send ("send all waiting at once")

When a turn completes with **2 or more** deferred messages, they go out as **one batch frame** rather than racing the per-message intake, so the server coalesces them into a single merged turn (one reply) while keeping each as its own transcript row. In `drainQueueOnFrame`, `canBatch = sendable.length >= 2 && sendable.every(i => !isSlashText(i.text))` — a slash command is a hard server-side coalescing barrier, so any deferred slash item (or a lone item) falls back to individual `sendToSession` calls. The batch path calls `sendBatchToSession` → `chatWs.sendMessages(sessionId, [...])`, which emits a single `kind: 'messages'` WS frame carrying every message (each with its own `platform_msg_id` for optimistic-row reconciliation + dedup). On the backend this maps to `RouterInbound::Batch → UserInputBatch → handle_merged_user_turn` (one coalesced turn). Content-less junk in `deferred` (only possible from an out-of-band `localStorage` write — the composer/edit paths refuse blank items) is filtered out first so it can't wedge the queue or skew the batch threshold. The batch optimistically appends N pending user rows + one turn-arm, mirroring `sendToSession`'s bookkeeping; a disconnected send rolls back the fired mark and leaves items deferred for retry.

If `turn_state{active:false}` arrives and the `message` branch did **not** dispatch the deferred items this turn (a blank/tool-only/errored/cancelled turn emits no assistant `message`), the still-pending deferred items are moved back to the parked queue (`restoreDeferred`) — visible/editable and drained on the next completion — rather than stranded as read-only thread bubbles. Restoring (not sending) here also ensures a turn ended via `/stop`/error can't auto-fire a deferred item ahead of the pause-setting notice.

### localStorage persistence (two-context store, blob refs)

Each session's queue persists under the key `aura.queue.<sessionId>` (`QUEUE_KEY_PREFIX = 'aura.queue.'`) as a JSON `SessionQueue` `{ items, deferred, pauseReason }`. `writeQueue` removes the key entirely when the queue is fully empty (`items` + `deferred` empty and `pauseReason === null`). On load, `loadAllQueues` scans every `aura.queue.*` key; `readQueue` validates via `isValidQueue` (rejecting malformed blobs to an empty queue) and tolerates a **missing `deferred`** field (coerced to `[]`) so a queue persisted before that field existed still loads. A `storage` event listener ingests writes from **other tabs** of the same origin (the event never fires in the writer tab) to keep sidebars/panels in sync cross-tab.

Attachments persist as **blob refs** (`WireAttachment` = `{ kind, blob_id, mime_type, size, filename }`) — the blob itself stays server-side; previews re-fetch by `blob_id` with the channel token (`AttachmentImage` in the panel, since `<img>` can't carry the auth header). So a reload keeps a parked or still-deferred message including its attachments.

**Two-context store** (`queueStore.tsx`): `QueueApiContext` holds the imperative, referentially-stable mutator handle (`enqueue`/`removeItem`/`editItem`/`reorder`/`popTop`/`takeItem`/`deferItem`/`removeDeferred`/`restoreDeferred`/`setPause`/`clearPause` + `queue(sid)` live ref read) and **never** re-renders its consumers on queue change — it's read by the WS `onFrame` closure. `QueueStateContext` holds the reactive `QueueMap` that the panel, composer decision, and sidebar badge subscribe to. `apply()` updates `queuesRef.current` **synchronously** before `setQueues`, so two mutations in the same tick (resume = `clearPause` then `popTop`; auto-fire peek then `popTop`) compose off the first instead of clobbering through a stale render-time ref.

## Thread rendering & turn lifecycle

The transcript is a flat array of `TranscriptRow`s (`web/src/pages/ChatPage.tsx`, type at lines 68-128) held per-session in `SessionView.transcript`. The list renders inside a centered `max-w-4xl` reading band (`flex flex-col gap-3 ... mx-auto`, ~line 2247); each row is a `MessageBubble` (line 4043). A `TranscriptRow` is one of: a user/assistant **message bubble**, a `kind === 'work'` **work block**, or a `notice` row — `MessageBubble` dispatches on `row.kind === 'work'` → `WorkBlock`, then `row.notice` → notice card, else a message bubble. Rows are keyed by a synthetic `key` (`hist-<sid>-<ordinal>` for REST history, `stream-…`/`msg-…`/`pending-…`/`notice-…` for live rows); ordinal-keyed history keys let a reconnect replay reconcile against rows already on screen instead of duplicating them. All source below is `web/src/pages/ChatPage.tsx` unless noted.

### Message bubbles: layout, alignment, attachments

`MessageBubble` (line 4043) splits on `row.role === 'user'`. The outer wrapper is `items-end` for user, `items-start` for assistant; the inner column is `w-fit` capped at `max-w-2xl` (user) / `max-w-4xl` (assistant) so bubbles shrink to content. **User bubbles** carry a 2px border, horizontal padding, and a 60%-opacity brand-gold fill (`border-2 border-black px-3 bg-brand/60 shadow-brutal-sm`); **assistant replies are borderless prose** on the canvas with no horizontal padding, so the text sits flush at the band's left edge. User text renders as `font-mono whitespace-pre-wrap` plain text (markdown is deliberately *not* applied to user input so paths/hashes/HTML show verbatim); assistant text renders through `MarkdownBody` (`ReactMarkdown` + `remark-gfm`, brutalist component overrides in `MARKDOWN_COMPONENTS` ~line 3902, `.chat-prose` class). Markdown is gated on `!isUser && !row.notice && body.length > 0`.

Each bubble's timestamp sits at its bottom-left (`self-start`), formatted by `formatTimestampShort` (line 3868: `HH:MM` same-day, else `MM-DD HH:MM`) with a full locale tooltip from `formatTimestampTooltip`. `createdAt` is the persisted `session_messages.created_at` for history rows; for live WS frames (the wire shape omits it) it is the receive time, stamped at first delta rather than at the final `Message` so the shown time matches when the bubble appeared.

**Inline attachments** render via `AttachmentList` (line 4008) inside the bubble, above the body: `kind === 'image'` → `AttachmentImage` thumbnail (the blob is fetched with the channel token and shown via an object URL, since `<img>` can't carry the auth header — see `web/src/pages/chat/AttachmentImage.tsx`), other kinds → a named `RiFileLine` chip. This needs live attachment details (`row.attachments`, present on optimistic sends + WS frames); REST history rows carry only `hasAttachments`, so `body` falls back to a literal `[attachment]` placeholder string when details are empty (line 4089).

**Copy-to-clipboard**: an assistant reply's timestamp row carries a hover-revealed `CopyButton` (line 4464) to the right of the time, rendered only for `!isUser && !row.streaming && body` (no copy on user bubbles or still-streaming text). It calls `navigator.clipboard.writeText(body)` and swaps `RiClipboardLine`→`RiCheckLine` for 1200 ms. The button is `opacity-0 group-hover:opacity-100`.

### Streaming answer bubbles

A live reply streams into a standalone `streaming: true` assistant bubble *below* any open work block. Wire deltas don't write the DOM directly — a per-session **rAF pacer** (`streamPacersRef`, `pacerTick` ~line 495) accumulates incoming text into a `target` and reveals it adaptively (`step` 2 chars for a small backlog up to `ceil(backlog/6)` when >400 behind, lines 507-511) so the reveal reads as a steady typewriter trickle without lagging the server. `appendStreamingDelta` (line 3178) / `writeStreamingAnswer` (line 3156) maintain the trailing streaming row. While streaming, a blinking caret (`w-1.5 h-3 ... bg-current animate-pulse`, line 4125) follows the plain-text body; markdown is *not* applied mid-stream (the per-char reveal already signals progress and a caret after a code fence/list would render off). The terminal `Frame::Message` finalizes the same bubble in place via `finalizeMessage` (line 3582), clearing `streaming` and applying markdown.

### Work / reasoning blocks (collapsible)

A tool-using turn folds its intermediate progress — reasoning, tool calls, status lines, and mid-turn prose — into one collapsible `kind === 'work'` row (`WorkBlock`, line 4249). Each block holds an ordered `steps: WorkStep[]` (type line 57). **Live**, the block is a bordered card (`shadow-brutal-sm`) headed by a spinning `RiLoader4Line` + "Working" + a 1 s-ticking `LiveElapsed` counter (line 4394, suppressed below 1 s so it never reads "Working 0s"). A live turn with no step yet is just the compact spinner card; the steps panel grows in (grid-rows `0fr`↔`1fr` animation) only once the first step lands. **On completion** the block collapses to a dim, left-flush `Worked Xs ›` summary (`formatWorkedLabel`, line 4237 — sub-second turns drop the duration to bare "Worked"); the two display flags (`boxed`, `panelOpen`) come from the pure `workBlockDisplay` (line 4220). A collapsed block is followed by a faint 1px full-width divider (`border-t border-black/20`, line 4386) that disappears when re-expanded. The collapsed summary is click-to-toggle (`expanded` state); live and "settling" blocks are non-toggleable. A block that produced **no steps** (a direct answer) is dropped entirely on close (`closeActiveWork`, line 3406), so a collapsed block always has work to show and the `›` arrow is always meaningful.

While a turn streams, the steps panel auto-pins to its tail (`useLayoutEffect` + `stepsPinnedRef`, lines 4278-4292) so the newest line stays visible — but only while the user is at/near the bottom (48px slack); scrolling up disengages the pin.

Step rendering (`WorkStepView`, line 4159): **reasoning** → `✻` + italic dim mono (consecutive reasoning chunks merge into one paragraph via `appendReasoningStep`, line 3296); **status** → `⟳` + dim mono line; **prose** (mid-turn answer text the model emitted before its final reply) → full `.chat-prose` markdown like the answer bubble; **tool** → `⏺` + bold tool name + optional `(label)`, a spinning loader while `running`, and an indented `⎿` summary line colored by status (`error`→`text-err`, `denied`→`text-warn`, else dim). Tool steps are keyed by `toolCallId`: `pushToolStartedStep` (line 3322, idempotent on re-delivery) creates the running step and `applyToolCompletedStep` (line 3347) resolves it by call id, synthesizing a completed step if the start was never seen.

`ensureWork` (line 3240) is the fold engine: it locates (or opens) the turn's block and, if a streaming answer bubble is trailing, moves that text into the block as a `prose` step ahead of the new step — that is what makes "a progress frame interrupting the stream means the text so far was intermediate" work. Blocks are `role: 'system'` so they never collide with assistant-streaming reconciliation. On REST reload the gateway sends a `work` transcript item that `historyRowToTranscript` (line 3668) rebuilds into a finished (collapsed) block via the same `WorkBlock`.

### Status steps (compaction)

`Frame::Status` (handler line 2692) maps `phase` to a human line — `compacting`→"Compacting context…", `compacted`→"Context compacted", else the raw phase — and pushes it as a `status` step into the open work block (`pushStatusStep`, line 3383). It does not end the turn.

### Notices (info / warn / error)

A `notice` row renders a bordered card (`MessageBubble`, lines 4055-4080) tinted by level: `error`→`bg-err/10 border-err text-err`, `warn`→amber, `info`→blue, with a bottom-left timestamp. `Frame::Notice` (handler line 2983) is **terminal for the turn**: it closes the open work block (collapsing it to `Worked Xs`), sets `awaitingReply: false` and `turn: { active: false }` (closing the race so a late frame folds into the closed block rather than spinning a new one), then appends the notice card. Notices cover slash-command replies (`/compact` confirmation), refusals, and errors. `noticeLevel` (line 3619) normalizes the wire string (default `warn`). Persisted notices reload via the `kind === 'notice'` history branch at their stored `notice_level`.

### Progress observer notices (transient)

A `Frame::Notice` with `transient: true` is the out-of-band progress observer's mid-turn narration, **not** the turn's reply. It is folded into the open work block as a `status` step (line 2991), leaving the turn running. This is deliberate: treating it as terminal previously split one long turn into two separate `Worked Xs` blocks (the observer collapsed the block, then later tool activity opened a fresh one).

### Working indicator

Between `handleSend` and the agent's first output frame, `SessionView.awaitingReply` drives a standalone `WorkingIndicator` (line 4449) — a compact bordered "Working…" spinner card pinned below the transcript (line 2293). Its header matches the live work block's, so the two read as one continuous element once steps start landing. `awaitingReply` clears on the first `answer_delta`, any non-user `message`, a `notice`, `status`, `reasoning`, or `tool_*` frame, and on Reset / session switch.

### Tool approval prompts

When a tool needs approval, `SessionView.pendingApproval` (`PendingApproval`, line 130) drives an `ApprovalCard` (line 4526) rendered at the foot of the transcript (line 2294). The card shows `Approval needed: <tool>`, the first 8 chars of the call id, an optional description, a bulleted list of `ResourceAccess` entries rendered by `formatAccess` (line 4759 — e.g. `read <path>`, `run: <command>`, `reach <host>` / `network access`, `read env: …`), and a collapsible `<details>` with the raw `paramsPreview`. Three buttons emit an `ApprovalDecision` (line 147): **Approve** (`bg-ok`), **Approve always** (`approve_always`), **Deny** (`bg-err`). Buttons disable while `!connected` (a warn line explains the WS is down) and after the first click — a synchronous `submittingRef` guard (line 4543) blocks a double-fire before the optimistic parent dismiss unmounts the card. `receivedAt` lets the `pending_approvals_snapshot` reconciliation tell a stale pre-reconnect card apart from one that arrived in the subscribe/snapshot race window.

### Task checklist

`web/src/components/chat/TaskChecklist.tsx` renders the active session's planning checklist above the transcript (line 2239), driven by the idempotent `Frame::TaskList` snapshot (handler line 2713 replaces `SessionView.tasks` wholesale). It renders nothing when the list is empty. The header is a collapsible bar showing `Task List · <done>/<total> done`. Each `TaskRow` shows a status marker — `in_progress`→spinning brand loader, `completed`→green `RiCheckboxCircleFill` + struck-through dim text, `pending`→neutral empty box — the subject, and an `after #N, …` hint resolved from `depends_on` ids to 1-based positions (`positions` map) so the opaque id never leaks. `asStatus` narrows any unexpected wire status to `pending` (`TaskStatus` is `pending | in_progress | completed`).

### Turn state, cancellation, and mid-turn interjection

`Frame::TurnState` (`SessionView.turn = { active, startedAt }`, handler ~line 2738) is the server-authoritative "is a turn in flight, since when" signal, broadcast at every turn start/end and snapshotted on every Subscribe — so a tab that opened mid-turn or reconnected still shows the working state. `applyTurnState` (line 3540) reconciles the tail: `active:true` re-pins/re-opens a block whose `workStartedAt` matches the turn or opens a fresh empty one; `active:false` closes any open block (`closeActiveWork`), which is the turn-end signal that doesn't depend on a terminal `Message`/`Notice` arriving — so a turn that errors or ends with a blank reply can't leave the block spinning. A null `started_at` under `active:true` is treated as a stale artifact and ignored so a finished turn can't resurrect as a phantom "Working" box.

**`/stop` cancellation** has three reflections. (1) **Optimistic** (the tab that typed `/stop`, ~line 1428): `finalizeTrailingAnswer` (line 3464) keeps the cut-short streaming reply as its own non-streaming bubble, then `closeActiveWork(…, true)` collapses the block with `workCancelled`, so the summary reads `Cancelled · Worked Xs` (in `text-error`, `formatWorkedLabel` line 4237) instantly without a round-trip. (2) **Broadcast**: every tab's `Frame::Notice` handler detects a real cancellation via `isStopCancellationNotice` (line 3499, matching the `STOP_CANCELLED_NOTICE_MARKER` substring "Cancelled the in-progress reply"; a no-op `/stop` says "Nothing in progress to stop." and is not marked), and `markLastWorkCancelled` (line 3508) labels that turn's block so observers and reloads agree with the originator. (3) **No-notice cancels** (agent-loop abort, gateway shutdown mid-turn): `isCancelledWorkAt` (line 4419) detects a trailing closed work block while `turn` is known-inactive and renders a `CancelledTurnIndicator` (line 4430) — a warn-tinted "Cancelled" pill — right after it (line 2266). It renders only once this connection has a definitive `turn === { active:false }` (with `turn === null` it stays quiet, never mis-labeling a running turn) and only for the very last row.

**Mid-turn interjection**: when the user sends while a turn is active (`view.turn?.active`, ~line 1267), `settleActiveWork` (line 3439) relabels the open block "Worked Xs" (clearing `workActive` so it isn't a second live "Working…" next to the block the agent opens after the interjection) but keeps it **expanded** via `workSettling` so the split-off work stays visible until the turn fully ends; `closeActiveWork` clears the flag (→ collapse) at turn-end. `workBlockDisplay`'s `settling` arg makes a settling block render boxed with its panel open. An atomic "send all waiting at once" batch instead calls `closeActiveWork` (collapse) before opening the batch turn's block (~line 1345). The interjection queue panel, deferred-message bubbles, and the post-`/stop`/error pause banner are covered in the interjection-queue section (`web/src/pages/chat/QueuePanel.tsx`, `queueStore`).

## Connection, frames & data flow

The `/chat` page (`web/src/pages/ChatPage.tsx`) holds a single WebSocket to the gateway channel listener and fans every inbound frame into a per-session view bucket. This is the transport spine: connection lifecycle, channel-token auth, the `Frame` discriminated union, optimistic-send reconciliation, the bounded view cache, REST history hydration, and the per-session model pin. The transport client is `web/src/api/chatWs.ts` (`ChatWs`); the frame router is `routeInboundFrame` in `ChatPage.tsx`.

### Connection status & reconnect

The page surfaces a three-state `ConnectionStatus` (`web/src/api/chatWs.ts`): `{ state: 'connecting' }`, `{ state: 'connected' }`, `{ state: 'disconnected', retryInMs, lastError? }`. The composer's send control is disabled unless `status.state === 'connected'` and a channel token exists.

`ChatWs` owns reconnect entirely; the React layer never re-opens a socket. On any non-permanent close it schedules a reconnect with exponential backoff `RECONNECT_BASE_MS * 2 ** attempt` capped at `RECONNECT_MAX_MS` (1s → 30s). A successful `register_ack { ok: true }` resets the attempt counter. The single WS is tied to the **channel token, not the session id** (the WS-lifecycle effect deps on `channelToken`): it opens once a token exists and lives until the user leaves `/chat` (component unmount calls `ws.close()`, which sets `closed` and stops all reconnects).

Liveness: after `register_ack` the client runs a combined heartbeat/watchdog timer (`HEARTBEAT_TICK_MS` = 5s). Every inbound frame (including `pong`) stamps `lastFrameAt`. If `now - lastFrameAt >= HEARTBEAT_PING_INTERVAL_MS` (20s) it sends a `ping`; if `> HEARTBEAT_LIVENESS_TIMEOUT_MS` (45s) it treats the socket as half-open and force-closes it so the normal `onclose` → backoff path fires. The 20s ping cadence is deliberately under the typical NAT idle window. `buildWsUrl` upgrades `http(s)` → `ws(s)`, points at `/v1/channel-ws`, and passes the token as a `?token=` query param (browser WebSocket can't set auth headers).

### Channel-token auth & re-mint

The WS authenticates with a **channel token** minted per HTTP session, not the admin bearer. Bootstrap (`ChatPage.tsx` mount effect, ~558-699) picks an anchor session — URL `sessionId` if it names an existing session, else newest existing, else a freshly created one — then mints/refreshes its token (`POST /v1/chat/sessions/{id}/token`, or `POST /v1/chat/sessions` for a new one which returns a `channel_token` directly). The token is stored in `channelToken` state and feeds the WS effect. The same token authorizes attachment-blob fetches via an `x-aura-channel-token` header (`<img>` can't carry it, so images are fetched and shown via object URL).

A browser WebSocket can't read the HTTP upgrade status, so a revoked/expired token surfaces only as a frameless `onclose`. `ChatWs` handles both rejection shapes:
- Explicit `register_ack { ok: false }` → `suspended = true` (auto-reconnect paused), fire `onTokenRejected(reason)`.
- A close before `register_ack` increments `consecutivePreAckCloses`; at `PRESUME_TOKEN_DEAD_THRESHOLD` (2) in a row it presumes the token dead, suspends, and fires `onTokenRejected`.

The page's `onTokenRejected` handler (~705-730) re-mints against the anchor session (`POST /v1/chat/sessions/{anchor}/token`) and calls `ws.replaceToken(newToken)`, which clears `suspended`, resets backoff, and reconnects immediately. The mint retry uses its own exponential backoff (2s → 30s) and a generation counter (`tokenRemintGenRef`) so a newer rejection or unmount abandons an in-flight chain.

### Inbound frames & per-session routing

`Frame` is a `kind`-discriminated union mirroring `crates/channels/src/wire.rs` (full list in `web/src/api/chatWs.ts`). Frames carrying a `session_id` are routed into the matching `SessionView` bucket so **background (unviewed) sessions still accumulate state** — a delta for a non-active session reaches the right bucket without racing the active view. `ChatWs.onMessage` consumes `register_ack` / `ping` / `pong` / `reset` internally and forwards the rest to `onFrame`. Two layers process a forwarded frame: the WS-lifecycle `onFrame` callback in `ChatPage.tsx` (~746-904) does cross-cutting bookkeeping, then calls `routeInboundFrame` for the per-session transcript mutation.

Frames the web handles:

- **`message`** — the authoritative final text for a user echo or assistant reply. Replay rows (those carrying `ordinal`) are keyed `hist-<sid>-<ordinal>` and reconciled against existing rows (see reconciliation below); live rows without `ordinal` go through `finalizeMessage`. An assistant `message` closes the turn's open work block (`closeActiveWork`) and ends `awaitingReply`.
- **`messages`** — client→server only (batch coalesce, see optimistic send); not received.
- **`answer_delta`** — streaming final-reply text. Routed through the rAF **pacer** via `enqueueDelta`, not straight to `setViews` (`routeInboundFrame`'s `answer_delta` case via `appendStreamingDelta` is a defensive fallback). Dropped if the session is in `stoppedSessionsRef` (delta racing in after a local `/stop`).
- **`reasoning` / `tool_started` / `tool_completed` / `status`** — turn-progress frames folded into the trailing collapsible "work" block (`appendReasoningStep`, `pushToolStartedStep`, `applyToolCompletedStep`, `pushStatusStep`). Each first settles the paced answer bubble (`flushPacerKeepStreaming`) so buffered mid-turn prose folds into the block *before* the progress step, and each clears the final-reply phase. `status.phase` maps `compacting`/`compacted` to friendly text.
- **`notice`** — terminal turn output (slash-command reply, refusal, `/compact` confirmation) appended as a `system` row; closes any open work block and ends `awaitingReply`. A `transient: true` notice is the progress observer's mid-turn narration and folds into the work block as a status step instead (treating it as terminal previously split one turn into two work blocks). A non-transient `/stop`-cancellation notice marks the block "Cancelled", keeps the in-progress reply as its own bubble (`finalizeTrailingAnswer`), and adds the session to `stoppedSessionsRef`.
- **`attachment`** — tool-produced media (sent file, screenshot) rendered as its own standalone assistant bubble; deliberately not folded into the work block and not turn-ending.
- **`task_list`** — idempotent snapshot replacing the session's planning checklist wholesale (empty array hides the panel).
- **`turn_state { active, started_at }`** — server-authoritative turn lifecycle, broadcast at every turn start/end **and** snapshotted on every Subscribe, so a tab that missed the progress frames (opened mid-turn, reconnected) still knows the agent is working. Recorded on `view.turn` and reconciled into the trailing work block (`applyTurnState`); `active` ends `awaitingReply`. In `onFrame`, a fresh `active` clears `stoppedSessionsRef`/`streamingAnswerRef` and re-arms the interjection auto-fire token.
- **`approval_requested` / `pending_approvals_snapshot` / `approval_resolved`** — drive `view.pendingApproval`. The snapshot (one per Subscribe) reconciles a local card: drop it only if it pre-dates `lastConnectedAt` *and* is absent from the snapshot's `call_ids`, protecting cards that arrived in the subscribe-vs-snapshot race window. `approval_resolved` walks every bucket (call_id→session is unknown client-side) and clears the matching card.
- **`session_updated { patch }`** — sparse `SessionPatch` (`created_at`/`last_active`/`hidden`/`pinned`/`folder_id`) merged into the sidebar list via `applySessionPatch` (order never reshuffled; `hidden:true` removes the row and `releaseSessionView`s the bucket).
- **`session_activity { source, at }`** — cheap unread/freshness signal delivered to **every** connection regardless of subscription. Foreground session doesn't bump; background gets a `+1` badge. An activity for an unknown session id nudges the CronInbox to refetch (cron-spawned sessions are filtered out of the chat list).
- **`folders_changed { folders }`** — full folder-tree snapshot; replaces the local tree wholesale via `folderStore.replaceFolders`.

`history_snapshot` / `start_bot` / `stop_bot` / `slash_manifest` / `subscribe` / `unsubscribe` / `register*` / `reset` are not expected at `routeInboundFrame`'s default arm (mostly stripped before `onFrame`, or handled in `ChatWs`).

### Optimistic user rows, idempotency & reconciliation

A composer send (or queue auto-fire) goes through `sendToSession` (~1251), which **immediately appends an optimistic user row** (`pending: true`, `key: pending-<clientMsgId>`) and sets `awaitingReply`, then transmits. The reconciliation/idempotency key is a single `crypto.randomUUID()` (`clientMsgId`) that doubles as the WS frame's `platform_msg_id`. It serves two jobs:
- **Idempotency** against the gateway's `InboundDedup`: a re-send after a WS drop, double-click, etc. carrying the same id is rejected inside the recency window instead of producing a second turn.
- **Reconciliation**: the live user echo (`message`, role `user`, with matching `platform_msg_id`) replaces the pending row **in place** — clears `pending`, adopts the server's (possibly sanitized) text, keeps the React key so the bubble doesn't remount (`routeInboundFrame` `message` case, ~2915-2941). Echoes without a matching placeholder (sibling tab, post-Reset) fall through to `finalizeMessage`.

Catch-up **replays** (a `message` with `ordinal` set) are keyed `hist-<sid>-<ordinal>` so React reconciles them against REST history rows of the same shape — a duplicate replay is a no-op. Because the gateway zeros `platform_msg_id` on replay, leftover local rows from a drop are matched heuristically: a trailing `streaming` assistant row is swallowed by the finalized assistant message; a `pending` user row is matched by text; a finalized live row is matched by role+text+`hasAttachments` within the last `TAIL_WINDOW` (16) rows, iterating oldest→newest so duplicate text pairs with the correct ascending ordinal. Sending identical text twice inside the drop window can mis-match (one duplicate row) — no worse than the pre-fix baseline.

The "send all queued at once" path (`sendBatchToSession`, ~1328) appends N optimistic rows and sends one `messages` batch frame (`ChatWs.sendMessages`); the server runs them as a single coalesced turn while keeping each as its own row, so they merge deterministically rather than racing per-message intake. Each item keeps its own `clientMsgId` for the same reconciliation/dedup.

### Bounded per-session view cache

The tab keeps one `SessionView` per visited session in a `views` map (`SessionView` / `EMPTY_VIEW`, ~168-226): `transcript`, `pendingApproval`, history flags (`historyLoaded`/`historyLoading`/`olderLoading`), pagination cursors (`oldestOrdinal`/`hasMore`), `awaitingReply`, the per-session `model` pin, `tasks`, and the server-authoritative `turn`. Switching sessions does **not** drop the prior transcript. `VIEW_CACHE_LIMIT` = 20 caps the map: when exceeded, the LRU effect evicts the oldest non-active buckets (by frame recency in `recencyRef`), each via `releaseSessionView` (drops the WS subscription, frees the bucket, clears recency). The active session is never evicted. Only `views`-mutating frames bump recency — sidebar-only signals (`session_updated`) don't bias retention. Revisiting an evicted session re-subscribes and re-fetches via REST.

### REST history hydration vs live frames

Subscribe is **sticky**: the active-session effect (~958) calls `ws.subscribe(sessionId)` and keeps the subscription when the user switches away, so background sessions keep accumulating frames. On first visit (no `historyLoaded`) it lazily fetches `GET /v1/chat/sessions/{id}`, mapping each DTO row through `historyRowToTranscript` (~3668; `message`/`work`/`notice` kinds, keyed `hist-<sid>-<ordinal>`). History rows carry only `has_attachments` (the DTO omits attachment detail) so they render an `[attachment]` placeholder, unlike live/optimistic rows which carry full `attachments`.

The WS replay cursor is seeded from the REST response: `recordOrdinal(sessionId, -1)` (sentinel forcing full replay even for an empty transcript) then `newest_ordinal` — so a reconnect after a network dip asks the server, via `Subscribe.since_ordinal`, for anything newer rather than dropping it. `oldest_ordinal`/`has_more` drive scroll-up pagination (`loadOlder` with `before_ordinal`, prepending a slice while pinning scroll position by `scrollHeight - scrollTop`). Live `answer_delta`/`message` rows have no server ordinal and don't move `oldestOrdinal`.

**Reset path**: a server `Frame::Reset` (cursor implied a catch-up gap past the replay cap, or an indeterminate stream) means the live stream is stale. `ChatWs` clears its per-session cursors before firing `onReset` (reusing the offending cursor would just trigger another Reset). The page wipes each view to `EMPTY_VIEW` (preserving only `pendingApproval`, a transport-independent state), cancels all pacers, and bumps `historyEpoch` to force every view to re-hydrate from the authoritative REST source — the cheapest safe convergence since some gap rows may already be in the transcript and can't be told apart.

### Per-session model pin

The header model picker is a per-session pin (`SessionView.model` ↔ `session.state.last_llm`): `null` = follow `default-llm`, a string = a pinned `aura.json` entry name. The option list and default come from `GET /v1/llm/models` at bootstrap. `model` is seeded from the GET-session detail's `last_llm` on history load. `handleSelectModel` (~2097) persists via `PUT /v1/chat/sessions/{id}/model` with `{ llm: name }` and, on success, sets `view.model` to the server-confirmed `last_llm`; on failure it appends an error notice and leaves the pin unchanged. Persistence is server-authoritative; the live route follows on subsequent turns.

## Cron inbox & navigation shell

### Global icon rail

`components/IconRail.tsx` renders the app's primary navigation: a narrow (48px / `w-12`) solid brand-amber, icon-only vertical bar mounted once in `App.tsx` for every authenticated route, to the left of the content `<main>`. It replaces the old text sidebar.

Layout, top to bottom:
- **Chat** (`RiChat3Line`) — the primary destination, linking `/chat`. Rendered as a distinct bordered tile separated from the rest by a hairline rule (`border-t-2 border-black/25`). Its tooltip is `Chat · Aura v<version>` when a `version` prop is passed, else just `Chat` (`version` is threaded down from `App.tsx`).
- **Admin surfaces** — a `<nav>` mapping the `DESTINATIONS` array, in order: `Log` → `/logs` (`RiFileList3Line`), `Trace` → `/traces` (`RiGitMergeLine`), `Cron` → `/cron` (`RiAlarmLine`), `Jobs` → `/jobs` (`RiStackLine`), `Analytics` → `/analytics` (`RiBarChartBoxLine`), `LLM` → `/llm` (`RiCpuLine`).
- **Logout** (`RiLogoutBoxRLine`) — pinned to the bottom (`mt-auto`); calls `logout()` from `useAuth()`. It is a `<button>`, not a route link.

Each label surfaces only as the native `title` hover tooltip; the rail shows icons exclusively. Links use react-router `NavLink`, so the active route's tile gets the pressed/active styling (`railActive`: `bg-surface`, hard shadow, push-down on `:active`) vs. the idle hover treatment (`railIdle`).

Note: the `/cron` route reached from this rail is the cron-*job* admin page (`CronPage`), which is separate from the in-chat **CronInbox** notification panel described below.

### CronInbox right panel

`components/CronInbox.tsx` is a right-side notification pane rendered inside the chat view only (`ChatPage.tsx` mounts `<CronInbox refreshSignal={cronInboxRefresh} />`). It is fixed to the right edge (`absolute right-0 top-12 bottom-0`, `w-[260px]`) and **only shown at the `xl` breakpoint and wider** (`hidden xl:flex`). It surfaces cron-fire output that is deliberately kept out of the main session sidebar: cron mints a fresh session per trigger, and those sessions are filtered out of the chat list server-side, so without this panel a cron fire would be invisible.

**User-facing behavior.** Each row is one cron fire (`ChatCronMessage`, keyed by `session_id`), showing the originating `cron_job_id` (truncated to 8 chars), a relative fire time (`formatRelative`: `HH:MM` today, `Mon DD` within a week, else `YYYY-MM-DD`; absolute `fired_at` is in the row `title` tooltip), and a one-line summary (the assistant `response` if present, else the `prompt`, collapsed to one line and capped at 96 chars). Clicking a row expands it to show the full `Prompt`, `Response` (rendered `(pending)` when the agent hasn't replied yet), and the full `session_id`; clicking again collapses it. The header shows a count of unread fires and a **mark-all-read** badge; an unread fire carries a left brand accent bar and a `new` chip. A refresh button forces an immediate refetch.

**Data + polling.** Rows come from `GET /v1/chat/cron-messages` (`ChatCronMessagesList`) via the admin client. The panel fetches on mount and on a `POLL_INTERVAL_MS = 30_000` (30s) interval — chosen against the scheduler's ~10s tick so the panel stays within roughly one fire of staleness. A `reqGenRef` generation counter discards stale in-flight responses. The `refreshSignal` prop (bumped by `ChatPage` when a `session_activity` WS frame arrives for a `session_id` the main list doesn't know — the signature of a cron-spawned session) triggers an immediate out-of-band refetch instead of waiting for the next poll tick; `refreshSignal === 0` is ignored so the initial mount value doesn't double-fetch.

**Unread state is client-only.** There is **no server read-state** for cron fires. Acknowledged fires are tracked entirely in `localStorage` under `SEEN_KEY = 'aura.cron.seen'`, a JSON array of acknowledged `session_id`s, mirrored into React state (`seen: Set<string> | null`). A fire is acknowledged when its row is opened or collapsed (`toggle` → `markSeen`) or via mark-all-read (`markAllSeen`). `unreadCount` is the count of list items whose `session_id` is not in `seen`.

**`null` baseline invariant (subtle).** `seen === null` means "no baseline yet" (first run / unparseable storage) and is deliberately distinct from "baselined, nothing unread" (empty set). On first run, once items load, every fire currently in the list is written to `seen` as already-seen, so opening the panel for the first time does **not** flag the entire backlog as new; only fires that arrive after that baseline are unread. While `seen` is `null`, `unreadCount` reports `0` and no row shows the `new` accent. `localStorage` write failures are swallowed (`writeSeen` catch) — read tracking still works in-memory for the session but won't persist.
