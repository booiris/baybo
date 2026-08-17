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
2. **Session sidebar** (`pages/chat/SessionSidebar.tsx`): a newest-first conversation list of compact single-line rows, organised into a **two-level folder tree** (`@dnd-kit` drag-to-file, with create / rename / delete-dissolve — deleting a folder never deletes its chats), a lifted **Pinned** block on top, and a trailing **Uncategorized** bucket — plus the New-chat button and a coral-highlighted active row. Right-clicking a conversation row opens a context menu (move to folder / **rename** / pin / hide); rename swaps the row for an inline title input, so no third hover button is added to the slot below. Each row's right slot shows a mono relative timestamp that on row hover **swaps to a pin/unpin + hide (delete) button pair** (no space is reserved for it); badges that persist *without* hover share that slot too — an unread count (background sessions), a parked-interjection-queue count, and an approval-pending dot. Row titles are regular weight; the **active row's title is bold**.
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
  roving would advertise an interaction that is not there. It stretches: a
  chip in a row takes the default, a **form field** passes `className="w-full"`
  and `panelClassName="left-0"` so the trigger fills its row and the panel
  spans the same width. Every chosen value on the board goes through it — the
  issue rail, the create modal's chip row, and the new-agent form's framework
  and llm — so no screen here draws an operating-system menu.

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
group — mark read, filter, the lead chat, activity, settings — and the board
itself starts directly under it. There is no second toolbar row: a strip that
existed to hold four filter controls cost every board 30px of column height to
show controls that are set once and then looked past.

`BoardFilterMenu` is that collapse. The trigger goes gold and carries a count
of active narrowings, so a board holding cards back always says so from the
header — a hidden filter that silently empties a column is the one failure
mode collapsing the strip could introduce, and the badge is what rules it out.
`boardFilter.ts` owns the list of narrowings (`restrictionCount`), because the
badge and the Clear button must never disagree about what counts as filtered.

## The column page

One column of the board as a whole page — `/projects/:pid/board/:status`
(`pages/projects/ColumnPage.tsx`), reached from the maximize button in each
column's header. The board's columns are honest about what they are — five
narrow queues — and a queue holding thirty cards reads as a smear of two-line
tiles; this is the same column at reading width. Each card is **one flat row**
in a single bordered surface, rows separated by 1px rules (the activity
drawer's "a stream, not a stack of cards"), the card's whole vocabulary laid
out in aligned cells: priority mark, `#number`, the full title, the Blocked /
Run-failed badges, the branch chip, the sub-issue ring, working / queued /
held, the assignee's face, the relative time, and the unread pill. The cells
come from
`cardChrome.tsx`, which both the board's tile and this row read — one home, so
the two views of a card cannot drift apart.

The header keeps the whole board one press away: a row of five stage tabs
(`COLUMN_PILL_LABEL` + the board's live counts, the current one gold), the
same `BoardFilterMenu`, and a New-issue button scoped to this column. The
filter is the **same URL vocabulary the board reads**, and the back link and
the tabs carry the params along — the narrowing survives the zoom in, across
stages, and back out. A tab whose stage holds something new wears a **red
dot, not a number** — pressing a tab cannot discharge what it shows, opening
cards does — and the open stage's tab stays bare, because its rows carry the
counts themselves (`columnHasNews`, the same predicate `readingOrder` lifts
by, so the dot and the lift cannot disagree).

It stays the board in every rule that matters, deliberately: the reading
order (pin, then unread) is applied once to the fetched board and never
written; a drag reorders within the column and sends `persistedOrder` (then
`withPositions` writes it back); a `project_changed` frame that lands
mid-drag is held until the card does; `dropRejection` still refuses
unassigned work into In Progress; the pin is the same press the board's tile
takes; and the assignee's face opens the agent profile in the same sliding
right-hand layer (`FloatingPanel.tsx`, shared with the board for exactly this
reason). The one thing a single-column page cannot do — drag a card to
another column — the row's status `Picker` does instead, appending the card
to the end of the destination's stored order, exactly as a drop on a
column's body would.

Two mechanics the row's shape depends on. The row div is also the keyboard
sensor's **activator node**, engaging dnd-kit's `event.target !== activator`
guard: Enter on a control inside the row — the picker, the pin, the face —
presses that control, instead of silently lifting the row and eating the
press. And below `sm` the assignee and time cells go; they are the widest
facts the title can spare, and the always-on set has to fit a phone with the
title still worth reading.

## Where a notification is a number and where it is a dot

Two levels, and the difference is whether the thing you press can discharge
what it shows.

The **rail's Projects entry carries a bare red dot**. It used to carry a
count of boards, and a count there is a promise the click cannot keep: the
entry opens exactly one board — the last one visited — so the number
survived everything the operator did on the board they actually landed on,
which is what "the red dot won't go away" always turned out to mean. The
switcher's own trigger wears a second dot when a board *other* than the open
one is lit, because that dropdown is the only thing on screen that says
which board; its rows keep their per-board counts.

The **card carries the number**, top-right of its header row, in the same red
pill the rail used to wear. Every number there is one card away from zero:
opening the card stamps `issues.read_at` and the count goes. It counts an
agent's comments and an agent handing the card back into Review — never the
operator's own, since your own words and your own tidying are not news to
you.

The **header carries the board's total**, on the Mark-read button, and it is
allowed to be a number for the same reason the rail's is not: the press is
what empties it. It counts every card the board holds, including the ones the
filter is hiding, because that is what the press clears — a number that only
counted what got through the filter would leave cards lit on a board the
operator was told read zero. The button stays in the group when the total is
zero, greyed and dead, rather than disappearing: it vanishes exactly when it
is pressed, and a control that goes away under the press that emptied it
slides the next button into the press after it.

Two more ways to work through what is new, both of them view-only. The filter
menu's **Unread only** narrows to cards carrying a count — it counts as a
narrowing, so the header's badge admits to it like any other. And every column
**hoists its unread cards to the top**, which is a reading order laid over
`position` and never a rewrite of it: within each half the operator's own
order survives intact, and a card falls back into it as soon as it is read.
The lift happens on its own — the board refetches on a timeline frame
precisely because that is when a count changes, so an agent's comment raises
its card while the operator is looking at the column. That is the feature, not
a side effect. It is applied to the board the fetch produced rather than to
the rendered view, so there is only ever one column order and the drag uses
it — see "Dragging a card". A **cancelled** card is never lifted: cancel is
terminal, and floating a struck-through card over live work because somebody
spoke on it before it was called off is the board arguing with itself.

The press reaches further than "unread" sounds like it does, in two ways. The
hover text carries one of them — it clears cards the filter is hiding. The
other is not on screen anywhere: the same `read_at` cursor carries the rail's
*unseen failure*, so one press also stops the rail counting failures nobody
has opened. The cards keep their `✕ Run failed` marker, so this is the
divergence that already exists, in its documented safe direction — the rail
going quiet over a board that still shows the failure, never the reverse.

A card whose newest run failed says so on its face (`✕ Run failed`, the
Blocked badge's shape in the error tone) and is reachable through the filter
menu's **Failed run only**. That marker is not a read cursor and no amount of
looking clears it — its detail page's **Run again** is what does. Before it
existed the rail counted failures on a board where no card admitted to one,
so finding the card the badge meant took opening them one at a time.

## A dot is for news; a stopped board is not news

The rail's dot means **an event is waiting on you** — an approval, a failed
run, an agent's comment. It does *not* mean "this board has a problem", and
the difference is why runs the daily ceiling is holding were taken out of it.

A hold is a standing condition. It does not arrive, and it stops being true
only when the operator changes a number. Painted into one undifferentiated
dot — in the very same red the card's unread pill wears — it was
indistinguishable from a mark that could not be cleared, and that is exactly
how it got reported: *"the red dot won't go away."* The operator was right,
and reading the dot as an unread badge was the natural reading, because the
design gives news and stoppage one colour and one shape.

So the condition moved to where you can act on it. `OverCeilingChip` joins the
board header's right-hand action group whenever `burnState(...) === 'over'`,
wearing `HEADER_ACTION` in the warn tone, a `RiPauseCircleLine`, and the
figures — `$6.10 / $5.00 · 602k / 100k` — with the sentence in its title.

Two things the mark is deliberately not. Not `⚑`: that is already the board's
mark for *blocked*, on the card badge, the timeline and the issue rail, and
one mark cannot mean two things on a board where cards really do get blocked.
And not a unicode glyph at all — the card badges are drawn in unicode but this
group's icons come from Remix, so a bare character here would be a third
vocabulary in one row.

**Both** ceilings are named when both are set (`boardMeters`), and only the
one that is biting is at full strength. `boardMeter` picks a single ceiling to
*speak in*, which is right for a sentence and wrong for a readout: money and
tokens are independent gates, either stops the board, and an operator shown
only the tighter of two that are both spent raises one and watches nothing
happen. Pressing it opens project settings.

A **chip in the group**, and deliberately not a banner across the board. The
first cut was a warn-tinted strip under the header, and it was wrong for a
reason worth keeping written down: a board over its ceiling is over it for
twenty-three hours of every day, so a strip is up ~23/24 and is furniture by
the second morning — the dot's failure again, in a louder register. The
header's right-hand group is already where everything acting on the whole
board lives, and `BoardFilterMenu` is the precedent for the shape: a control
that tints and carries a figure exactly when the board is holding something
back. It also costs no column height, which is what killed the second toolbar
row in the first place.

The figure being on screen **at rest** is the trade the bare dot could not
make, because a dot has nowhere to put a number. On the column page the chip
drops its press — that page has no settings modal, and the board is one press
away on the back link it already has.

The trade is real and was taken deliberately: a frozen board no longer says so
from the rail, so a board you are not looking at goes quiet. What still
reaches across boards is the project switcher's per-board meter, which shows
`602k / 100k` in the warn tone.

Two card-level things survive from when `held` was a rail signal, and both
earn their place independently. The card says `held` in the run cell both
views share (`cardChrome.RunWord`), in the warn tone the issue page's own run
chip wears — it used to print `queued` there, because `runIndicator` answered
a three-value question with two, so a card the budget had stopped claimed to
be waiting for a free slot on a board where every slot was free. And the
filter menu's **Held on budget only** finds those cards among thirty, which a
word in a meta row cannot. That narrowing is the one whose fact is **not on
the card's row**: it comes off the board's live runs through `runIndicator`,
the very call the card's word reads, so `filterBoard` resolves it per card and
hands it to `matches`. Deriving it a second time server-side onto the card DTO
would have been two sources for one fact on two refresh schedules, which is
the drift `CardSignals` exists to prevent.

`useAttention` is one module-level store with one timer and an
`invalidateAttention(client)` — not a `useState` + `setInterval` per
component. The minute-long poll used to be the *only* refresh, so a signal
the operator had just discharged stayed on screen for up to a minute; every
act that clears one now asks the server again. Every act: the project-settings
save is the one that releases a held run, and it was the last one still
bumping `refreshKey` without asking the rail — so the fix for the dot left the
dot up, and read as the fix not having worked.

## Pinning a card

Above the unread lift sits one the operator asks for. A **pinned** card is
read first in its column, and `readingOrder` is where both lifts live because
they are one order: **pinned, then unread, then the board's own** — with the
unread lift applying again *inside* the pinned block, so a pinned card
carrying a comment leads it. The rank is the whole idea. A pin is what
somebody chose; an unread count is what happened to a card while they were
elsewhere, and what was chosen outranks what arrived.

It is a reading order and nothing else, held to exactly the promises the
unread lift makes. It never writes `position` — a drag still sends
`persistedOrder`, the stored order — and it is deliberately absent from the
board's `(priority, position, number)` pick order, which is how work leaves
Todo. `priority` is already the field that says "do this first"; a pin that
also promoted a card would give one question two answers, and pinning a card
merely to keep an eye on it would quietly jump it past urgent work. The cost
is the one the unread lift already charges: a second boundary a dragged card
settles back across on the next refetch.

The two lifts disagree about cancelled cards, on purpose. An unread count
never lifts one; a pin does. Nothing but the operator put the pin there, and
a control that goes on offering itself while quietly refusing to work is
worse than a struck-through card at the top of a column.

The pin **is** the marker — one element, not a badge beside a button.
`cardChrome.PinButton` draws a filled pushpin on a pinned card and nothing at
all on an unpinned one until the card is hovered or the button is tabbed to,
because a permanent outline glyph on every card is furniture. It leads the
card's meta row rather than taking the top-right corner, which belongs to the
unread count. It has to stop the press twice over: the card's own click opens
the issue, and the whole card is also the drag handle, so `pointerdown` is
stopped as well or a press that drifted 4px picks the card up.

That makes the card a shortcut, not the only door — hover is a thing a touch
screen does not have. The issue's own rail carries a **Pinned** row that is
always on screen, a press like the Status/Priority/Assignee rows above it
rather than a fact like Parent and Blocked. Both write the same
`PATCH { pinned }`, and both are optimistic: the card moves under the press
and a failed write puts the board back and says so, because the pin is the
one mark here whose truth is the server's.

Nothing is written to the timeline. A pin changes nothing about the work —
priority, which genuinely decides what the board starts, is silent too.

## A card's three properties

Status, priority and assignee are set in two places — the issue page's rail and
the create modal's chip row — and both read them out of
`pages/projects/issueFields.tsx`: one list of what the choices are, one idea of
what each looks like. Before it existed the modal had already grown a second
`PRIORITY_LABEL`, which is one stale rename from a board where the same card
says two different things.

Each wears the board's own vocabulary rather than a word: status is the
column's pill in its column colour, priority carries its tone, and an assignee
is a **face** and a handle. All four chips are `Picker`s — the create modal's
were native `<select>`s, drawing an operating-system menu in the middle of a
hand-drawn modal, which is the same thing the rail was fixed for. Parent and
stage share one chip, because a step's stage means nothing without the parent
it is a step of.

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

## Sticking to the newest edge

The chat thread and a card's detail pane are read the same way — downwards, and
answered at the bottom — so they hold their newest edge by one rule, and
`components/scrollPin.ts` is where it lives. A surface is **pinned** while the
reader is within `BOTTOM_SLACK_PX` of the bottom (`atBottom`); pinned, an
arrival scrolls them to it, and un-pinned it does not touch their scroll and
raises a pill instead — *New messages* in the thread, *New activity* on a card.
Pressing the pill, or **sending**, re-pins: your own message is the one arrival
always worth being taken to, wherever you were when you wrote it.

Two details the posture depends on. Only a moved **tail** raises the pill: a
board frame re-fetches a whole card and a scroll-up page prepends to a
transcript, and neither is news below the reader. And `useHoldBottomEdge`
watches the **content** box for height that lands after the commit — an
attachment thumbnail is fetched by its own component, so a comment with a
screenshot grows ~96px a beat after it renders, which on a cold open left the
reader parked just above the newest entry with no pill to say so.

The pill needs a positioning context outside the scroller: an absolute box
inside one is placed against the content and scrolls away with it. On a card
that is a wrapper around `<main>`, because the card's composer — unlike the
thread's floating pill — is `sticky`, and on screen only while the timeline is.

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

Targeting is **a point, always** — `pointerWithin`, and for the keyboard sensor,
which has no cursor at all, the middle of the dragged card. The trade is worth
naming: the cursor sits wherever the card was grabbed, so a card held by its far
edge leads its own outline. The aim is the cursor.

Two seams belong to no card — the 8px between cards and the column's side
padding — and a column hit means "append to the end", so leaving them to the
column would flick the preview to the bottom on every boundary swept past. In a
seam the nearest card by centre wins instead. Below the last card is the one
place the column is the honest answer, and it stays that way.

**A point over nothing decides nothing, and the drop still lands.** Three rules
hold that together, and the board crashed without them. The **column droppable
is the whole `<section>`** — border, header, list — because a cursor is aimed at
a droppable or at nothing, and with only the list answering, the band across
every column header was board that took no card, which is exactly where a card
headed for the front of a queue is aimed. What is still nobody's is the 12px
between two columns and the margin around them; there the preview simply
**holds** its last position. And a release over nothing **writes the preview**
rather than rolling it back — the preview is the promise the drag makes, and a
card that sat in Review for the length of a drag and then snapped home because
the button came up over a seam reads as the board having eaten the move. Escape
still cancels.

The rule those replace was a fallback to `rectIntersection` whenever the cursor
was over nothing, and it is worth knowing why it cannot come back. It scores by
overlap with the **dragged rectangle**, which rides the cursor and therefore
cannot see the board re-flow underneath it — so the answer became a function of
the preview it had just produced. Moving the card to the target vacated its old
slot, which promoted the card behind it into the rectangle's band, which flipped
the answer back; dnd-kit fires `onDragOver` on every change of target and needs
no pointer event to do it. One mouse move into that band was worth 27 previews
at ~5 commits each, and React threw "Maximum update depth exceeded" (#185) at
50 — the whole page, gone, mid-drag. `dragConvergence.test.ts` sweeps every
layout, grab and pixel of the board for a target that is not a fixed point of
that exchange, `boardDrag.test.tsx` drives the real page through the real
dnd-kit over a faked layout, and both keep the old rule beside the new one so
they cannot pass for the wrong reason.

Two smaller things the same drag depends on. A `project_changed` frame that
lands mid-drag is **held** until the card does: a refetch replaces every column
wholesale, re-keys the cards and re-ranks the very column being dragged in, so
answering it under a live drag resolves the drop against a layout nobody aimed
at. And each column's `SortableContext` `items` is memoised on its cards rather
than rebuilt per render — dnd-kit keys its whole context value off that array
and compares it by identity, so a fresh one each render re-renders every card in
the column and leaves the sort animation permanently switched off.

**Two pieces of feedback, not three.** Which column it will land in is the
column's brand outline and `bg-brand/15` tint; where in that column is the
dragged card itself, which `onDragOver` moves into position and renders at 40%
for the length of the drag. There is deliberately no dashed placeholder: the
one that used to sit under each column's list was pinned to the column's end
regardless of the real insertion point, so on any drop that was not an append
it contradicted the preview sitting a few cards above it.

**One order is rendered and dragged; a different one is written.** The unread
hoist is applied to the board the fetch produced rather than to the rendered
view, so the eye and `resolveDrop` are reading the same column and a drop
lands where it was aimed. What the move *sends* is `persistedOrder`: the
column in stored `position` order with the dragged card lifted out and put
back in front of the anchor `resolveDrop` already resolved. So the hoist stays
a reading order even across a drag — otherwise one drag in a column somebody
had just commented on would bake that comment into `position`, which is half
of `(priority, position, number)`, the order the board takes work out of Todo
by. `withPositions` writes the sent order back onto the cards, so a second
drag before the board refetches does not send slots the first one replaced.

What this costs is the one drop the hoist will not let stand: a card dragged
across the unread boundary — a read card above an unread one, or an unread one
below a read one — moves in the stored order and then settles back where the
hoist puts it on the next refetch. The hoist owns the top of a column for as
long as anything in it is new, and the alternative is a comment permanently
re-ranking a column. Every other drag, cross-column ones included, lands where
it was aimed and stays.

Two earlier shapes, both wrong, worth knowing about. Holding the hoist in the
view and dropping it for the length of a drag put the un-hoist in the same
batched commit as dnd-kit's own drag start, so a card could change slots
inside its column before the first `over` resolved — and since `resolveDrop`'s
insert-before/insert-after correction only cancels itself for an *adjacent*
target, the leftover was a real one-slot move: a 4px twitch on a card nobody
had dragged posted a reorder. Sending the rendered order instead is the other
one, and it is the quieter of the two — nothing looks wrong at the time, and
the column stays re-ranked after the cards are read.

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

The **team strip** in the board header is faces and nothing else — 26px, a live
status dot on every one (green working, grey idle), a gold halo on the lead, a
dashed `＋` at the end. A teammate with nothing going but something waiting is
dimmed rather than plainly idle, and `teamModel.agentRunStates` is the one
place that decides which: **working** outranks the waits, because an agent on
three cards is not idle whatever is stacked behind them, and among the waits
**held** outranks **queued**, because a queued run starts on its own when a
slot frees and a held one waits on somebody raising a ceiling. A held run used
to answer to neither of the two sets that function replaced, so a board the
budget had stopped drew a full strip of grey idle dots with nothing on it to
say why. The handle rides in the tooltip: sixteen named pills are
wider than the header they sit in, and the face is what the operator already
recognises on every card. Removal is not on the strip; it is on the profile the
face opens, where it asks twice.

Whether an avatar carries that dot is the caller's `dot`, not something
`Avatar` infers from `run`. Most callers have no run data at all — a comment's
author, a picker's option — and a grey dot there would be the component
announcing "idle" on their behalf. The strip asks for one on every seat,
because a roster where only the busy have a dot cannot be told from one whose
dots failed to load; a live run turns it green wherever it appears.

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
