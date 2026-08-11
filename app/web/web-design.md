# Baybo Web Design System

This document outlines the design principles, visual identity, and component patterns used in the Baybo Dashboard.

## Design Aesthetic: Neo-Brutalism

The Baybo Dashboard follows a **warm** "Neo-Brutalist" design language characterized by:

- **Warm, high contrast:** A cream canvas with warm near-black ink/borders and an amber action color (a deliberate warm-yellow lean, not the original cool blue).
- **Heavy Borders:** Consistent use of 2px or 3px solid borders. `border-black` is revalued to warm near-black (`#2a2520`) globally, so existing utilities pick up the warm tone without a per-component sweep.
- **Hard Shadows:** Off-set warm-black shadows without blur (`shadow-brutal`).
- **Geometric Rigidity:** Minimal use of soft rounded corners (standard radius is 6px).
- **Technical Typography:** Monospace by default to emphasize the "system" nature of the tool; the chat thread renders assistant prose in Inter sans (`.chat-prose`) for readability, keeping mono for metadata/code.

## Visual Identity

### Color Palette

| Name            | Hex       | Usage                                          |
| :-------------- | :-------- | :--------------------------------------------- |
| **Canvas**      | `#faf6ec` | Primary background (warm cream)                |
| **Surface**     | `#fffdf7` | Raised panels / cards (revalues `white`)       |
| **Ink**         | `#2a2520` | Primary text + borders (revalues `black`)      |
| **Ink Soft**    | `#6b6258` | Secondary text and metadata                    |
| **Brand**       | `#f2c14e` | Warm gold — primary actions, active states, the icon rail, and the user message bubble (at 60%). Pair with `text-ink` (dark on gold), never `text-white` |
| **Brand Hover** | `#f5b400` | Action hover states                            |
| **Selected**    | `#f08a5d` | Warm coral-orange for the *selected* row (chat + agents sidebars). Light enough that **dark `text-ink`** reads best on it (same dark-on-warm pairing as the brand) |
| **Error**       | `#e53e3e` | Destructive actions, Error logs                |
| **Warning**     | `#dd6b20` | Warning logs                                   |
| **Info**        | `#3182ce` | Informational logs                             |
| **OK**          | `#2f855a` | Success states                                 |
| **Violet**      | `#6b46c1` | Categorical only (trace step kinds) — never a status colour |
| **Magenta**     | `#b83280` | Categorical only (trace step kinds) — never a status colour |

> The gold brand is light, so **dark `text-ink` on `bg-brand`** is the readable pairing (white-on-gold fails contrast). `black`/`white` are revalued in the `@theme` block of `index.css`, so the pervasive `border-black` / `bg-white` utilities warm automatically.

### Typography

- **Primary Font:** `Space Mono` (Monospace) - Used for body text, UI labels, and data.
- **Secondary Font:** `Inter` (Sans-serif) - Used as a fallback or for specific readability needs.
- **Styles:**
  - Headers: Bold, uppercase, tight tracking.
  - UI Labels: Bold, uppercase, tracking-wider.

### Shadows & Borders

- **Shadow Brutal:** `4px 4px 0 0 #2a2520`
- **Shadow Brutal SM:** `3px 3px 0 0 #2a2520`
- **Radius:** `6px` (`radius-brutal`)
- **Borders:** `2px` or `3px` solid warm near-black (`#2a2520`).

## Layout Structure

### 1. Authentication Layer

- **Login Screen:** Centered "brutal" box containing brand identity and token input.

### 2. Main Dashboard

- **Global icon rail (`components/IconRail.tsx`):** A narrow (48px) solid-amber, icon-only vertical bar mounted on every route (replaces the old 180px text sidebar). Chat is the primary destination (a chat-bubble mark, `RiChat3Line`); admin surfaces (Log/Trace/Cron/Jobs/Analytics/Agents/LLM) sit below; logout is pinned to the bottom, with the PWA install button directly above it when the browser offers one. Labels surface as hover tooltips.
- **Content Area:** Scrollable main view with `bg-canvas`.

### 3. Chat (three-zone, app/mac-style)

The `/chat` route renders inside the shell as three zones. (This section covers the **layout**; for the full chat feature set — folders, pin, the interjection queue, slash completion, input history, turn rendering — see [`../../docs/web-chat.md`](../../docs/web-chat.md).)

1. **Icon rail** (zone 1, the global rail above).
2. **Session sidebar** (`pages/chat/SessionSidebar.tsx`): a newest-first conversation list of compact single-line rows, organised into a **two-level folder tree** (`@dnd-kit` drag-to-file, with create / rename / delete-dissolve — deleting a folder never deletes its chats), a lifted **Pinned** block on top, and a trailing **Uncategorized** bucket — plus the New-chat button and a coral-highlighted active row. Each row's right slot shows a mono relative timestamp that on row hover **swaps to a pin/unpin + hide (delete) button pair** (no space is reserved for it); badges that persist *without* hover share that slot too — an unread count (background sessions), a parked-interjection-queue count, and an approval-pending dot. Row titles are regular weight; the **active row's title is bold**.
3. **Thread + floating composer:** the transcript renders in a **centered, symmetric `max-w-4xl` reading band** (`mx-auto`), centered on the **thread column** — the space beside the sidebar — with equal gutters either side at every width. Within the band, **all agent-side content (reply bubbles, work/reasoning blocks, working indicator, notices) is left-aligned** to the band's left edge — so a short reply starts at the left rather than floating in the middle, and a notice's left edge lines up with the reply bubbles. **User messages are right-aligned** to the band's right edge. Bubbles shrink to fit their content (agent content capped at the band width, user bubbles at a narrower `max-w-2xl`) and each bubble's timestamp sits at the bottom corner on the side its message is aligned to — **agent replies bottom-left, user messages bottom-right** (a notice keeps its bottom-left). **User messages carry a 2px border**; **agent replies are borderless prose** on the canvas (no horizontal padding, so the prose sits flush at the band's left edge). A **collapsed** work/reasoning block is followed by a faint, thin (1px) full-width divider tight under its `Worked …` summary — it disappears when the block is expanded. The agent reply text, its timestamp, and the `Worked …` summary all share the band's left edge. An agent reply's timestamp row carries a **hover-revealed copy icon** to the right of the time. **Message attachments render inline**: images as **thumbnails** (the blob is fetched with the admin bearer and shown via an object URL, since `<img>` can't send the auth header), other files as **named chips**. A thumbnail is height-capped, so clicking it opens a full-screen **image viewer** (`pages/chat/ImageLightbox.tsx`) over that same object URL — wheel/pinch zoom about the cursor, drag to pan (clamped to the image's edges), double-click for 1:1, download, `Esc` to close. Its chrome is a **real row**, not an overlay: a `shrink-0` bar (filename + dimensions left, controls right) over a `flex-1` clipped stage, so no zoom level can put the picture behind the toolbar. This needs the live attachment details (optimistic sends + WS frames); rows reloaded from REST history carry only `has_attachments`, so they fall back to an `[attachment]` placeholder.

In the chat view the **thread sits on `surface`** (`#fffdf7`) while all **side panels and the header bar use the cream `canvas`** (`#faf6ec`) — the session sidebar and the header. (The icon rail keeps its brand amber.) The composer's fade backdrop fades to `surface` to match the thread. Below it sits a **floating rounded composer pill** at the same `max-w-4xl` width as the band, on the same center axis, so its left/right edges line up with the agent and user message bubbles. The pill's footer carries (left) an **attach button** that uploads images/files to `POST /v1/blobs` (with the web's admin bearer) and shows each as a removable chip — image attachments preview as thumbnails; and (right) the **model badge** — a model picker shown only when more than one model is configured — sitting just left of the **send button**. The send button turns into a **red stop button** (issuing `/stop`) only while a turn is in flight **and the composer is empty**; with a draft typed mid-turn it stays a send button that **parks** the message into the per-session interjection queue (type `/stop` to stop instead). `Shift+Enter` inserts a newline; `/`-slash autocomplete pops above (`Tab` accepts, `↑`/`↓` pick; an empty composer recalls input history). Directly above the pill, an **interjection-queue panel** (`pages/chat/QueuePanel.tsx`) surfaces parked messages (reorder / edit / fire / remove) and, after a `/stop` or error, a **pause/resume banner**; "deferred" (send-after-reply) messages render as dimmed pending bubbles below the agent's output. A **gradient backdrop** (transparent at the top → opaque `surface`, matching the thread) sits behind the pill, **scoped to the conversation band** (it's a child of the composer form, so it spans the band width rather than the full thread, tinting only the column the bubbles occupy): the thread scrolls *behind* the composer and bubbles **fade out gradually** as they slide into it (fully gone by roughly the pill's middle), while the area below the input box stays clear — the floating look is preserved, no hard mask.

## Components

### Buttons

- **Standard Button:** Large, bold uppercase text, black border, brand background for primary.
- **Icon Button:** Square or circular, 2px border, used for compact actions.
- **Interaction:** Buttons often "push down" (translate 2px/2px) on active state, removing the shadow to simulate a physical press. Every button also needs a **hover** state — on a flat cream surface with no gradients or elevation, hover is the only thing that separates a live control from a printed label.
- **The pointer cursor is global, not per-button.** Tailwind v3's preflight gave `button` `cursor: pointer`; v4 dropped it, so a `<button>` shows an arrow unless it asks. Half this codebase's buttons carry a hand-written `cursor-pointer` and half were written after the upgrade and silently lost it. `index.css` answers it once in `@layer base` for `button`/`[role=button]` that are not `:disabled` — new buttons should not repeat it, and a `cursor-*` utility still overrides it from the utilities layer.

### Inputs

- **SearchBox:** Minimalist, with search icon and 2px border.
- **SelectBox:** Custom styled to match the brutalist aesthetic.
- **Picker** (`pages/projects/Picker.tsx`): a value you can press. The current
  value *is* the trigger — a status pill, a priority word, a face — wearing a
  caret, and the panel under it is a brutalist popover, not the OS dropdown.
  It replaced a transparent native `<select>` laid over the value, which drew
  an operating-system menu in the middle of a hand-drawn rail and gave the
  value no sign it could be pressed at all. Options may carry a `node`, so the
  status panel lists the board's own pills in column order and the assignee
  panel lists faces. Deliberately **not** `role="listbox"`: the options are
  ordinary buttons reachable by Tab, and claiming the role without arrow-key
  roving would advertise an interaction that is not there.

### Popovers

A popover is a `relative` wrapper, a trigger, and an absolutely-positioned
`border-2 border-black rounded-md bg-surface shadow-brutal` panel offset
`calc(100%+6px)` from the trigger — never an OS menu and never a modal. It
closes through `components/useDismiss.ts`, which owns two rules the panels kept
getting wrong on their own: Escape is **stopped** (these open inside surfaces
that close on Escape themselves, so one press would otherwise collapse the
whole stack), and the focus goes back to the trigger instead of onto the body.
The chat sidebar's own menus predate the hook and still hand-roll it.

## Board header

Everything that acts on the **whole board** lives in the header's right-hand
group — filter, the lead chat, activity, settings — and the board itself
starts directly under it. There is no second toolbar row: a strip that existed
to hold four filter controls cost every board 30px of column height to show
controls that are set once and then looked past.

`BoardFilterMenu` is that collapse. The trigger goes gold and carries a count
of active narrowings, so a board holding cards back always says so from the
header — a hidden filter that silently empties a column is the one failure
mode collapsing the strip could introduce, and the badge is what rules it out.
`boardFilter.ts` owns the list of narrowings (`restrictionCount`), because the
badge and the Clear button must never disagree about what counts as filtered.

## The board's right-hand layer

The activity drawer and the agent profile share one layer, are mutually
exclusive, and never navigate away from the board. `FloatingPanel` owns how
they arrive and leave: they **slide in** from the right (180ms, `motion-reduce`
respected) and leave the same way on **✕, Escape, or a press anywhere outside**
— the three the design specifies, kept in one place because both panels are
reached from the same avatars and buttons.

Two details the shape depends on. The panel mounts off-screen and moves on the
next frame, since one that mounts at its resting place has nothing to
transition from. And the parent's unmount waits out the slide, which means a
panel replaced mid-slide — a second avatar pressed while the first profile is
still leaving — must not take its replacement down with it: the profile panel
is keyed by agent id so the outgoing instance unmounts and cancels its own
pending close.

## Activity drawer

A **stream, not a stack of cards**: each entry is one flat row separated by a
1px rule, never its own bordered surface. A row is a tone dot, one sentence,
and a right-aligned relative time.

The dot is the point. `timelineModel.eventTone` maps every entry to the board's
six-colour vocabulary and `TONE_DOT` renders it — the same alphabet the card's
own timeline rail uses, which is why both live in `timelineModel.ts` rather
than in either component. The drawer is skimmed down its left edge before any
of it is read, so a failed run and a hire have to be distinguishable without
parsing either sentence.

The sentence is `feedLine`, **not** `describeEvent` with the actor bolted on
the front. `describeEvent` writes for a pane that is one card, so it can say
"moved it to Review"; in a board-wide feed that names nothing. Every feed line
names its card. Who acted, which card, and the one word that says how a run
ended are bold; everything joining them is not — which is why `feedLine`
returns spans rather than a string, so the sentence is not marked up in one
place and written in another. A comment is narrated, never quoted: an agent's
run report is hundreds of words that would bury every line around it.

The drawer slides in before its first fetch answers, so the wait is on screen.
It is filled with **placeholder rows in the real rows' geometry** rather than
left empty or given a spinner — an empty frame that fills a moment later lands
as two separate events, and a spinner throws the rows away and puts them back.
The row frame is one constant shared by a real line and its placeholder,
because a skeleton is only worth anything if nothing moves when it is replaced.
Only the first load shows them: a refresh keeps the rows it already has.

A settled-run line carries what the run took and cost — `run #1 done on #7 ·
2m10s · $0.04`. Both numbers are **derived server-side** over the run's own
cost window, by the same query the execution log uses, and arrive on the feed
entry rather than in the stored event: a run's cost is not a column anywhere,
because a session is shared by every run the same agent does on a card and only
the window attributes a call. A copy frozen into the timeline entry at settle
time would also be written before the run's last cost record necessarily is.
Absent is not zero on either — a run nobody claimed has no window, and `0s ·
$0.00` would report that as instant and free.

The drawer is **read-only**. Approvals appear here with a warn dot and are
answered on the issue's own timeline; there are no action buttons in the feed.

## Dragging a card

The drop target is decided by `pages/projects/dropTarget.ts`, not by one of
dnd-kit's built-in strategies, for two reasons that both showed up as "the card
is most of the way in and nothing happens".

`useSortable` registers a **droppable under the same id as the draggable**, so
the slot a card was lifted out of stays in the running and sits at exactly the
height the dragged rect is at — it wins every distance measure until the card
has fully cleared its old column. `resolveDrop` refuses a card landing on
itself, so this never produced a wrong drop, only a dead drag. The active id is
excluded from the candidates.

Targeting is then **cursor-first** (`pointerWithin`, falling back to
`rectIntersection` for the keyboard sensor, which has no cursor). The trade is
worth naming: the cursor sits wherever the card was grabbed, so a card held by
its far edge leads its own outline. The aim is the cursor.

Two seams belong to no card — the 8px between cards and the column's side
padding — and a column hit means "append to the end", so leaving them to the
column would flick the preview to the bottom on every boundary swept past. In a
seam the nearest card by centre wins instead. Below the last card is the one
place the column is the honest answer, and it stays that way.

**Two pieces of feedback, not three.** Which column it will land in is the
column's brand outline and `bg-brand/15` tint; where in that column is the
dragged card itself, which `onDragOver` moves into position and renders at 40%
for the length of the drag. There is deliberately no dashed placeholder: the
one that used to sit under each column's list was pinned to the column's end
regardless of the real insertion point, so on any drop that was not an append
it contradicted the preview sitting a few cards above it.

## Iconography

- **Library:** [Remix Icon](https://remixicon.com/) (via `react-icons/ri`).
- **Style:** Outlined or filled depending on emphasis, usually `text-xl`.

## Agent faces

Every agent has a portrait, and the order is always **uploaded avatar → the
bundled brand image (built-in profile only) → a generated Bottts robot**. The
robot is drawn locally by `src/components/botttsFace.ts` (`@dicebear/core` +
`@dicebear/bottts`), never fetched from `api.dicebear.com`: the dashboard is
served off a box that need not have an internet route, and a request would
hand an agent's id to a third party for a picture we can draw ourselves. It is
placed on the board's own warm tints rather than DiceBear's saturated palette.

The seed is the agent **profile id** — the only identity the board's roster
(`TeamMemberDto`) and the `/agents` page (`AgentProfileDto`) share, and the
only one that survives a rename. Seeding on the handle or the name would give
one agent two faces.

The portrait frame is **1px**, against the 2–3px this design system uses
everywhere else. A board chip is 18–26px across, and a 2px ring on an 18px
circle is a fifth of its radius — it read as chrome around a picture rather
than as the edge of one. The board's other small round furniture (the team
strip's status dot, the profile header's) already sits at 1px, so this is the
existing idiom for things this size, not an exception to the language.

Who draws what is resolved once per page in `pages/projects/portrait.ts`:
`useTeamPortraits(team)` fetches the roster's uploaded blobs (bearer-gated, so
they arrive as object URLs via `api/blobs.ts`) and falls back to the generated
face per agent. Components take the **resolved `src`**, never the blob id. The
operator and the board are not agents, get no portrait, and keep a monogram.

## App icon (PWA)

The installed app's icon is `assets/baybo.png` — the line-art robot, black on white, **not** restyled onto a brutalist gold tile. It is the same artwork the iOS app ships as its AppIcon, and one product should not wear two faces in a task switcher. The dashboard's neo-brutalism is the *interface's* language, not the brand's.

Every file in `public/` (`pwa-192`, `pwa-512`, `pwa-maskable-512`, `apple-touch-icon`, `favicon.ico`) is derived from that one source; the commands and the three non-obvious numbers in them are in [`../../docs/webui.md`](../../docs/webui.md).

## Update prompt

`src/pwa/PwaUpdateBanner.tsx` is the one piece of floating chrome outside a page: a gold pill (`bg-brand` + `text-ink`, the usual dark-on-warm pairing) fixed **bottom-right**, offering RELOAD when a newer bundle is installed and waiting. Bottom-right and not centre — the chat composer owns the bottom of the reading band at every width.
