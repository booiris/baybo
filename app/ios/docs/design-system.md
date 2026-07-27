# Baybo iOS — UI Style Guide

Governs the **visual design** of `app/ios` — a SwiftUI app whose screens, header, and
composer are native, with only the chat transcript rendered in a WKWebView. The system is
implemented twice: `app/ios/App/Support/Theme.swift` for the native chrome and
`app/ios/web/src/styles.css` for the transcript bundle. This file is the prose that explains
the intent so changes stay coherent. For app behavior/architecture, the root `/CLAUDE.md`
and the sibling docs under `app/ios/docs/` still apply.

## Identity

**Monochrome soft line minimalism**, light-only, ink-on-paper. Thin hairlines, large
rounded / pill corners, FLAT surfaces (no shadows), generous whitespace, lighter
weights — calm, airy, friendly.

It is tuned to harmonize with the **app icon**
(`app/ios/App/Resources/Assets.xcassets/AppIcon.appiconset`): a thin, rounded, friendly
line-art robot with a `>_` terminal glyph. **When in doubt, look at the icon** — the UI
should feel like it came from the same hand.

## Relationship to `app/web` — diverge on purpose

`app/web` is warm **neo-brutalism** (cream/gold, hard `--shadow-brutal` offset shadows,
boxy). This app **deliberately diverges**: soft, flat, rounded — to match its own icon
(the stronger brand signal for a phone app). Shared DNA is only **monochrome +
monospace**, plus token-NAME parity (`--color-ink`, `--color-paper`, …) so the palettes
read as one system.

**Do NOT** port web's brutalism here: no `shadow-brutal`, no hard offset shadows, no
sharp corners, no bold-everything.

## Where the system lives — two implementations, one system

The token set exists **TWICE**, and the two copies must be changed **together**:

- **`app/ios/web/src/styles.css`** — the `:root` block, for the transcript webview. Its
  header comment names the system and the token-name parity with `app/web`.
- **`app/ios/App/Support/Theme.swift`** — the Swift-side mirror (`Theme.paper`,
  `Theme.ink`, `Theme.line`, …) plus the button styles, for every native surface: the
  chat header, the composer dock, the chat list, settings, pairing, the viewers.

A colour changed on one side and not the other splits the app down the webview boundary,
which is invisible in any single screenshot of either half.

(The Deck webview is a separate surface with its own palette — the shell's `--deck-*`
tokens and the deliberately overridable card-base `--ink` / `--muted` / `--line` /
`--ok` / `--bad` — governed by `docs/modules/deck.md`, not by this token pair.)

### Tooling

- **Plain CSS. No Tailwind** — `styles.css` says so in its header comment. Only the
  transcript renders in the webview; everything else is native SwiftUI, so there is no
  utility-class layer and no `@theme` block. Tokens are ordinary custom properties on
  `:root`; write CSS classes that reference `var(--token)`.
- **Standalone pnpm project** — `app/ios/web` has its own `pnpm-workspace.yaml` and is
  intentionally NOT part of the root workspace. Run `pnpm install` / `pnpm build`
  (= `tsc --noEmit && vite build`) from that directory.

### Fonts are self-hosted — never a Google Fonts CDN

The rule is the app's offline / no-needless-network posture, and it holds on both sides:

- **Native:** Space Mono is **bundled as TTFs** in `app/ios/App/Resources/Fonts`
  (`SpaceMono-Regular.ttf`, `SpaceMono-Bold.ttf`, OFL) and registered via `UIAppFonts`
  in `project.yml`. `Theme.mono` serves it through `Font.custom("Space Mono", …)`, which
  falls back to the system face if the TTFs ever go missing.
- **Web:** Space Mono (400/700, latin + latin-ext) and Inter Variable ride `@fontsource`
  packages imported in `web/src/main.tsx` and bundled by Vite — imported BEFORE
  `styles.css` so the `@font-face` families exist when the theme references them. KaTeX's
  math fonts are self-hosted the same way (`katex/dist/katex.min.css`, woff2 emitted into
  the bundle and served by the transcript scheme handler), so math renders offline.

## Design tokens

From the live `:root` block in `app/ios/web/src/styles.css`:

```
--color-paper #ffffff   --color-surface #fafafa
--color-ink   #111111   --color-ink-soft #6b6b6b
--color-line  #e4e4e4 (light hairline)   --color-line-strong #bcbcbc
--color-err   #d40000 — the ONLY non-monochrome hue (destructive/error state only)
--font-mono   "Space Mono", ui-monospace, SFMono-Regular, monospace
--font-sans   "Inter Variable", "Inter", -apple-system, BlinkMacSystemFont, sans-serif
--radius 14px           --radius-pill 9999px
```

`--color-line` is for incidental hairlines (bubbles, chips) so the UI reads airy; ink is
reserved for text and deliberate line elements. `--color-line-strong` is the heavier
neutral rule (blockquote edges, dividers) — still neutral, still 1–2px.

The same `:root` block also carries the transcript's **layout knobs**, each with one
source of truth:

- `--attachment-max-h: 16rem` — the tallest an inline image renders. One source of truth:
  the `<img>` clamps to it, and a reserved box (`.attachment-bubble.sized`) has to solve
  the SAME cap or the box and the image it holds disagree — which is the shift the
  reservation exists to remove.
- `--chat-row-gap: 1.5rem` — the gap between message rows.
- `--msg-time-inset: 0.5rem` — how far a user message's timestamp is pulled in from the
  bubble's right edge. **Its own knob, NOT the bubble's padding**: tying the two together
  means re-aiming the clock silently re-pads every bubble.
- `--md-block-gap: 0.75rem` — markdown block rhythm.
- `--code-max-width: 130%` — how wide a code block may grow before it soft-wraps (see
  `.md pre code`). A **MULTIPLE OF THE READING BAND**, so the cap is really a bound on how
  far the block can be scrolled sideways: 130% = at most 0.3 bands of travel, about 58
  mono chars on a phone (the band holds ~47). **Deliberately not `ch`**: that unit resolves
  against the "0" glyph of the font in force when the style is computed, and the bundled
  mono webfont is still loading then, so WebKit substitutes the 0.5em fallback and the cap
  lands ~20% narrower than written (measured: 80ch → 62 chars).
- `--thread-top-inset: calc(env(safe-area-inset-top) + 58px)` and
  `--thread-bottom-inset: 0px` — see [Layout, safe areas, and the full-bleed webview](#layout-safe-areas-and-the-full-bleed-webview).

### Swift-side mirror (`Theme.swift`)

`Theme` holds the same palette as `Color` values — `paper` white, `surface` 0.98 grey,
`ink` `#111111`, `inkSoft` `#6B6B6B`, `line` `#E4E4E4`, `err` `#D40000` — plus:

- `Theme.radius = 14` — the CSS `--radius`, for in-plane inset boxes.
- `Theme.radiusModal = 20` — **floating layers only** (the confirm dialog). In-plane inset
  boxes keep `radius`; a floating card scales its corners with elevation, continuous
  curvature, or it reads sharp next to iOS 26 chrome.
- `Theme.mono(_:weight:)` — Space Mono, the chrome face.
- `Theme.sys(_:weight:)` — the system face (SF Pro), for surfaces that **deliberately** step
  out of the Space Mono chrome. Currently that is the chat list, which reads as a native,
  content-dense list rather than monospaced chrome.

## Rules

### Light-only

`:root { color-scheme: light }` in `styles.css` — pinned so iOS scrollbars/controls never
render a dark variant the design doesn't define. **No `@media (prefers-color-scheme: dark)`**,
on either side.

### Colour is for STATE, never chrome

Resting UI is strictly ink-on-paper. Red (`--color-err` / `Theme.err`) is the ONLY hue —
destructive/error only. Everything else is monochrome ink. No decorative hues.

This is why, for example, the message-index landing ring is an ink ring rather than a
coloured one, and why the failed-send glyph is the one red thing in the thread.

### Borders

**1px hairlines.** Light `--color-line` for incidental containers (inputs, bubbles,
chips); ink for deliberate line elements (the wordmark rule, focus). `--color-line-strong`
where a neutral rule needs more weight than a hairline (quote edges, dividers). **Never
heavier than 1px** for the incidental case.

### Corners

Always rounded — `--radius` / `Theme.radius` for cards/inputs, `--radius-pill` (SwiftUI:
`Capsule()`) for buttons & chips. **Never sharp.** Floating layers take `Theme.radiusModal`.

### Shadows

**None.** Surfaces are flat.

### Buttons

Primary = soft-filled ink **pill** (paper text, no shadow); `.danger` = red **outline**
pill; press feedback = `opacity .7 + transform: scale(.98)` (a gentle dim, not a push).
Fire a light haptic on primary-CTA press (`Haptics.tap()` — the style guide's physical
beat).

The native styles in `Theme.swift` are the concrete spec:

- **`InkPillButtonStyle`** — the primary CTA: soft-filled ink pill, paper text, REGULAR
  weight, uppercase with 0.18em tracking (plus the matching lead-in padding that keeps
  wide tracking optically centered — the CSS `text-indent` trick); press feedback is the
  gentle dim + scale.
- **`OutlinePillButtonStyle`** — the secondary CTA: ink outline pill with NO case transform
  and NO tracking (both reset on purpose — quiet next to the primary). `.danger` is this
  style with `color: Theme.err`. **A stroke-only pill needs an explicit
  `.contentShape(Capsule())`**: without a shape the transparent interior doesn't hit-test,
  leaving only the text column tappable.
- **`FilledPillButtonStyle`** — the base filled ink pill, regular weight, no case
  transform. What multi-button rows render as (pair confirm's Cancel | Pair).
- **`LinkButtonStyle`** — a quiet borderless text button (the direct form's "← Back"),
  `inkSoft`, pressed at 0.6. Its `minHeight: 44` + `.contentShape(Rectangle())` is the
  point: hit-test the padding, not the glyphs.

The compact in-card variant (the approval card's Approve/Deny) follows the same recipe —
mono(14), 44pt min height, capsule fill or stroke, the same dim + scale — and carries the
same stroke-only-pills-don't-hit-test contentShape.

### Typography

**Space Mono for ALL chrome; Inter for chat message bodies only.** In the transcript that
means `.bubble`, `.msg.assistant`, reasoning/prose work steps and the copy toast take
`--font-sans`; everything else (the work-block header, the loading line, chips) stays
`--font-mono`. Natively it means `Theme.mono` everywhere except the chat list, which is on
`Theme.sys` by design (above).

Make wordmark/CTA uppercase via CSS `text-transform` (SwiftUI: `.textCase(.uppercase)`) —
**not literal caps, because screen readers read true case** — with wide `letter-spacing`
(SwiftUI: `.kerning`); add matching `text-indent` (SwiftUI: an equal leading padding) so
wide tracking stays optically centered.

Panel rows carry `accessibilityLabel` = title and `accessibilityValue` = subtitle so
by-label UI smokes keep working and VoiceOver reads the pair (see
[model-picker.md](model-picker.md)).

### Layout, safe areas, and the full-bleed webview

Native SwiftUI owns all chrome and all safe-area handling for it. The transcript webview
is **full-bleed** (`viewport-fit=cover` in `web/index.html`) and pads itself instead:

- `--thread-top-inset: calc(env(safe-area-inset-top) + 58px)` — so the thread's oldest
  visible row starts below the native header (status bar + the 42pt glass bar), not under
  its icons. The status-bar part is device-correct via `env()`; the 58px covers the native
  bar height + breathing.
- `--thread-bottom-inset` — the height of the webview's bottom edge covered by native
  chrome (composer + the keyboard it rides). The webview NEVER resizes with the keyboard;
  native streams this value per layout tick over the bridge and the thread pads/pins
  itself, so content slides in lockstep. It **includes the home-indicator area** (measured
  to the screen edge), so no extra `env()`.

`.chat-log` is a plain growing block (`min-height: 100dvh`, no `overflow`) with
`justify-content: flex-start` — the document is the single scroller, and threads shorter
than the screen fill from the TOP down. See [transcript.md](transcript.md) for why.

### Native-app feel

Text selection, the iOS long-press callout, and the grey tap-highlight flash are disabled
globally on `:root` in `styles.css` (`user-select: none` + `-webkit-touch-callout: none` +
`-webkit-tap-highlight-color: transparent`; all three inherit, so `:root` covers the tree),
so the app doesn't feel like a web page. Images also get `-webkit-user-drag: none` so they
can't be drag-ghosted out of the app, and keep the callout off even inside now-selectable
message rows.

Selection/editing is re-enabled ONLY where content is genuinely readable text — the
transcript opts it back in on `.bubble`, `.msg` and `.work-steps` so message content is
copyable, and native `input` / `textarea` equivalents keep their own editing. **If a
control needs selectable text (e.g. tap-to-copy), opt it back in locally with
`user-select: text` rather than lifting the global rule.** The reverse also holds: a
surface that owns its own long-press turns the native callout back OFF locally
(`.bubble.user` does exactly that, so iOS's selection UI doesn't fire at the same ~500ms
and fight the custom copy — see [transcript.md](transcript.md)).

`-webkit-text-size-adjust: 100%` is set for the same "not a web page" reason: WebKit's text
autosizing boosts the font of any WRAPPING block laid out wider than the viewport, and the
code block — allowed to overrun the reading band up to `--code-max-width` — is exactly that
(it rendered ~1.5x, dwarfing the prose beside it). Nothing catches this from inside the
page: an absolutely-positioned measuring probe is its own autosize cluster and reads back
unboosted, and `getComputedStyle` reports the unadjusted size — only a screenshot shows it.
The transcript sets every size deliberately and is never pinch-zoomed, so opt out of the
whole mechanism rather than fight it per-block.

## Recorded deviations

The custom **Liquid Glass** surfaces — the chat composer dock, the jump-to-latest button,
and the Chats header's compose circle — are a deliberate, recorded departure from the flat
monochrome system (glass is neither flat nor strictly ink-on-paper). They are documented in
[navigation.md](navigation.md), which also holds the constraints they carry (white tint
only, borderless composer pill, bare glass on the discs) and the `glassSurface` shim every
custom glass surface must go through.

The system above still governs everything else.
