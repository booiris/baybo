# Aura Web Design System

This document outlines the design principles, visual identity, and component patterns used in the Aura Dashboard.

## Design Aesthetic: Neo-Brutalism

The Aura Dashboard follows a **warm** "Neo-Brutalist" design language characterized by:

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
| **Brand**       | `#ffd23f` | Sunny warm-yellow — primary actions & active states. Pair with `text-ink` (dark on yellow), never `text-white` |
| **Brand Hover** | `#f5b400` | Action hover states                            |
| **Selected**    | `#ff8c90` | Soft coral-pink for the *selected* chat row. Light enough that **dark `text-ink`** reads best on it (same dark-on-warm pairing as the brand) |
| **Error**       | `#e53e3e` | Destructive actions, Error logs                |
| **Warning**     | `#dd6b20` | Warning logs                                   |
| **Info**        | `#3182ce` | Informational logs                             |
| **OK**          | `#2f855a` | Success states                                 |

> The amber brand is light, so **dark `text-ink` on `bg-brand`** is the readable pairing (white-on-amber fails contrast). `black`/`white` are revalued in the `@theme` block of `index.css`, so the pervasive `border-black` / `bg-white` utilities warm automatically.

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

- **Global icon rail (`components/IconRail.tsx`):** A narrow (48px) solid-amber, icon-only vertical bar mounted on every route (replaces the old 180px text sidebar). Chat is the primary destination (the "A" mark); admin surfaces (Log/Trace/Cron/Jobs/Analytics/LLM) sit below; logout is pinned to the bottom. Labels surface as hover tooltips.
- **Content Area:** Scrollable main view with `bg-canvas`.

### 3. Chat (three-zone, app/mac-style)

The `/chat` route renders inside the shell as three zones:

1. **Icon rail** (zone 1, the global rail above).
2. **Session sidebar** (`pages/chat/SessionSidebar.tsx`): a flat, newest-first conversation list of compact single-line rows — New-chat button, coral-highlighted active row, unread badge. Each row's right slot shows a mono relative timestamp that **swaps to a delete button on row hover** (no space is reserved for it). Row titles are regular weight; the **active row's title is bold**.
3. **Thread + floating composer:** the transcript renders in a **centered, symmetric `max-w-4xl` reading band** (`mx-auto`). Within the band, **all agent-side content (reply bubbles, work/reasoning blocks, working indicator, notices) is left-aligned** to the band's left edge — so a short reply starts at the left rather than floating in the middle, and a notice's left edge lines up with the reply bubbles. **User messages are right-aligned** to the band's right edge. Bubbles shrink to fit their content (capped at the band width) and each bubble's timestamp sits at its bottom-left corner. **User messages carry a 2px border**; **agent replies are borderless prose** on the canvas (no horizontal padding, so the prose sits flush at the band's left edge). A **collapsed** work/reasoning block is followed by a faint, thin (1px) full-width divider tight under its `Worked …` summary — it disappears when the block is expanded. The agent reply text, its timestamp, and the `Worked …` summary all share the band's left edge. An agent reply's timestamp row carries a **hover-revealed copy icon** to the right of the time. **Message attachments render inline**: images as **thumbnails** (the blob is fetched with the channel token and shown via an object URL, since `<img>` can't send the auth header), other files as **named chips**. This needs the live attachment details (optimistic sends + WS frames); rows reloaded from REST history carry only `has_attachments`, so they fall back to an `[attachment]` placeholder.

In the chat view the usual panel/background relationship is **inverted**: the **thread sits on a near-white reading surface** (`--color-thread`, `#fffefc` — a touch whiter than the warm `surface`), while its **chrome — the session sidebar and the header bar — uses the cream `canvas`** (`#faf6ec`). The composer's fade backdrop fades to `thread` to match. Below it sits a **floating rounded composer pill** at the same `max-w-4xl` width as the band, on the same center axis, so its left/right edges line up with the agent and user message bubbles. The pill's footer carries (left) an **attach button** that uploads images/files to `POST /v1/blobs` (with the web's channel token) and shows each as a removable chip, plus the model badge; and (right) the **send button**, which becomes a **red stop button** issuing `/stop` while a turn is in flight (new sends blocked until it ends). `Shift+Enter` inserts a newline; `/`-slash autocomplete pops above. A page-colour **gradient backdrop** (transparent at the top → opaque `canvas`) sits behind the pill, **scoped to the conversation band** (it's a child of the composer form, so it spans the band width — never the full thread, so it can't paint over the right-hand panel/divider): the thread scrolls *behind* the composer and bubbles **fade out gradually** as they slide into it (fully gone by roughly the pill's middle), while the area below the input box stays clear — the floating look is preserved, no hard mask.

## Components

### Buttons

- **Standard Button:** Large, bold uppercase text, black border, brand background for primary.
- **Icon Button:** Square or circular, 2px border, used for compact actions.
- **Interaction:** Buttons often "push down" (translate 2px/2px) on active state, removing the shadow to simulate a physical press.

### Inputs

- **SearchBox:** Minimalist, with search icon and 2px border.
- **SelectBox:** Custom styled to match the brutalist aesthetic.

## Iconography

- **Library:** [Remix Icon](https://remixicon.com/) (via `react-icons/ri`).
- **Style:** Outlined or filled depending on emphasis, usually `text-xl`.
