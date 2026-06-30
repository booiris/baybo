# Baybo Mobile — UI Style Guide

Governs the **visual design** of `app/mobile` (the iOS companion — a Tauri WKWebView
running React + Vite). The single source of truth for tokens is `src/styles.css`
(`@theme`); this file is the prose that explains the intent so changes stay coherent.
For app behavior/architecture, the root `/CLAUDE.md` still applies.

## Identity

**Monochrome soft line minimalism**, light-only, ink-on-paper. Thin hairlines, large
rounded / pill corners, FLAT surfaces (no shadows), generous whitespace, lighter
weights — calm, airy, friendly.

It is tuned to harmonize with the **app icon** (`src-tauri/icons/icon.png`): a thin,
rounded, friendly line-art robot with a `>_` terminal glyph. When in doubt, look at the
icon — the UI should feel like it came from the same hand.

## Relationship to `app/web` — diverge on purpose

`app/web` is warm **neo-brutalism** (cream/gold, hard `--shadow-brutal` offset shadows,
boxy). Mobile **deliberately diverges**: soft, flat, rounded — to match its own icon
(the stronger brand signal for a phone app). Shared DNA is only **monochrome +
monospace**, plus token-NAME parity (`--color-ink`, `--color-paper`, …) so the palettes
read as one system.

**Do NOT** port web's brutalism here: no `shadow-brutal`, no hard offset shadows, no
sharp corners, no bold-everything.

## Tooling

- **Tailwind v4** via `@tailwindcss/vite` (mirrors `app/web`). Tokens live in the
  `@theme` block of `src/styles.css`; prefer CSS classes that reference `var(--token)`,
  with utilities for one-offs.
- **Standalone pnpm project** — `app/mobile` is intentionally NOT in the root
  `pnpm-workspace.yaml`. Run `pnpm install` / `pnpm build` (= `tsc --noEmit && vite build`)
  from this directory.
- **Fonts are self-hosted** via `@fontsource` (imported in `src/main.tsx`) — never a
  Google Fonts CDN (keeps the app's offline / no-needless-network posture). Space Mono
  (400/700) + Inter Variable.

## Design tokens (`@theme` in `src/styles.css`)

```
--color-paper #fff   --color-surface #fafafa
--color-ink   #111   --color-ink-soft #6b6b6b   --color-line #e4e4e4 (light hairline)
--color-err   #d40000 — the ONLY non-monochrome hue (destructive/error state only)
--font-mono   "Space Mono", ui-monospace, …
--font-sans   "Inter Variable", "Inter", -apple-system, …
--radius 14px        --radius-pill 9999px
```

## Rules

- **Light-only.** `:root { color-scheme: light }`. No `@media (prefers-color-scheme: dark)`.
- **Color is for STATE, never chrome.** Resting UI is strictly ink-on-paper. Red
  (`--color-err`) is the ONLY hue — destructive/error only. Everything else
  (including the scan-success dot) is monochrome ink. No decorative hues.
- **Borders:** 1px hairlines. Light `--color-line` for incidental containers
  (inputs, bubbles, chips); ink for deliberate line elements (the wordmark rule, focus).
  Never heavier than 1px.
- **Corners:** always rounded — `--radius` for cards/inputs, `--radius-pill` for
  buttons & chips. Never sharp.
- **Shadows:** none. Surfaces are flat.
- **Buttons:** primary = soft-filled ink **pill** (paper text, no shadow); `.danger` =
  red **outline** pill; press feedback = `opacity .7 + transform: scale(.98)` (a gentle
  dim, not a push). Fire a light haptic on primary-CTA press (`tapHaptic` in `App.tsx`).
- **Typography:** **Space Mono** for ALL chrome; **Inter** for **chat message bodies
  only** (`.bubble`). Make wordmark/CTA uppercase via CSS `text-transform` (not literal
  caps — screen readers read true case) with wide `letter-spacing`; add matching
  `text-indent` so wide tracking stays optically centered.
- **Layout:** the page wrapper class is **`.screen`** (NOT `.container` — that collides
  with Tailwind v4's built-in `container` utility). Respect safe-area insets: `.screen`
  folds top + bottom insets into its padding; overlays use `env(safe-area-inset-*)`.
- **Headings:** keep an explicit `font-size`/`font-weight` on base `h1` — Tailwind's
  Preflight resets headings to inherit, so un-classed `<h1>`s lose hierarchy otherwise.
- **Native-app feel:** text selection and the iOS long-press callout are disabled
  globally (`user-select: none` + `-webkit-touch-callout: none` on `:root`; images
  also get `-webkit-user-drag: none`) so the app doesn't feel like a web page.
  Selection/editing is re-enabled ONLY on `input` / `textarea`. If a control needs
  selectable text (e.g. tap-to-copy a pairing code), opt it back in locally with
  `user-select: text` rather than lifting the global rule.

## Must preserve (functional, not stylistic)

The QR-scan camera CSS in `src/styles.css` makes the windowed barcode scanner work: the
camera feed renders BEHIND the webview, so `html.scanning` (+ `html.scanning .screen`)
turns the page transparent, `.scan-warming` masks the warm-up frame, and `.scan-overlay`
/ `.scan-reticle` / `.scan-panel` provide the viewfinder + success beat. Restyle these
only with care — breaking them breaks pairing.

## Status

Redesigned: the **landing** (unpaired) screen. Deferred (palette/tokens already inherited,
layouts not yet polished):
- Propagate the system to the confirm-code / connected / chat screens.
- `.status` text is neutral; semantic error-red needs a `statusKind` state threaded
  through the pairing/scan flow.
- Add a primary (filled pill) vs secondary (outline pill) split — base `button` is the
  filled primary today, so multi-button screens show several filled pills.
