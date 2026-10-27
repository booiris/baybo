# Deck (iOS shell)

*`docs/modules/deck.md` is the source of truth for the Deck design; this document covers only the iOS half — `app/ios/App/Screens/DeckScreen.swift`, `app/ios/App/Core/DeckStore.swift`, `app/ios/App/Web/DeckBridge.swift`, and the `app/ios/web/src/deck/` shell.*

## The second webview

The Deck tab renders the app's SECOND webview (`DeckHost`), kept warm like
`TranscriptHost` (see [transcript.md](transcript.md)) and torn down with the
binding.

It loads `deck.html` — a second Vite entry in the SAME `app/ios/web/` dist the
transcript uses, so it rides the existing `app/ios/App/Resources/transcript/`
copy + scheme handler with no extra build step.

## The card sandbox

The shell (`app/ios/web/src/deck/`) draws the 2-column size-class grid and hosts
each card in an `<iframe sandbox="allow-scripts" srcdoc>` — opaque origin +
injected CSP, so **no network** — with a per-card `MessagePort` as the card's
identity.

## DeckStore

`DeckStore` (`app/ios/App/Core/DeckStore.swift`) is the engine:

- **REST refetch** over the active leg (`deckFetch`).
- **A `deck.json` mirror** for instant offline paint.
- **Live pushes** via the connection-global `DeckSink` (`DeckEventsRelay` — the
  `SessionActivityHandler` idiom). `Frame::DeckCardData` is accepted **iff its
  seq beats the cached one**; `Frame::DeckChanged` triggers a refetch.
- **Optimistic layout writes** with baseline rollback.
- **Op calls** (`deckCall`) correlated back to the card that asked.

Destructive card actions confirm **NATIVELY** — the shell only reports intent.

The native edit control is a glass circle: a pencil enters edit mode and a
checkmark finishes it. Its VoiceOver label follows the current action in both
the header and the side rail.

The empty board is native too. `DeckStore.isEmpty` follows the durable mirror
and each refetch; while it is true, `DeckScreen` covers the still-warm webview
with the shared `CreationPrompt`. That gives Deck the same SF Symbol, vertical
placement, typography, and CTA as Projects, Chats, and an empty project stage.
The CTA calls `AppStore.startCardDraft` directly. Its tracked setup session is
published by `DeckStore`, so the native button can show the in-flight state and
a second tap returns to the same chat.

The recycle bin is also native. Restore is the visible row action; permanent
delete is available from either a long-press menu or a trailing destructive
swipe with full-swipe disabled, followed by an explicit Cancel / Delete
Permanently alert. Both remove the row optimistically and restore it in place
if the active-leg request fails.

## Bridge

`app/ios/App/Web/DeckBridge.swift` ⇄ `app/ios/web/src/deck/bridge.ts`.

native→web:

```
init/deckState/cardData/bundle/callResult/pickResult/setEditMode/setLanguage/
restoreMaximized
```

web→native:

```
ready/refetch/requestBundle/call/pick/share/layout/cardAction/editMode/maximize/
haptic/log
```

- The empty-board CTA does not cross this bridge: it already lives in
  `DeckScreen` and calls `AppStore.startCardDraft` natively.
- `maximize` reports a card entered/left its full-screen layout so `DeckScreen`
  fades the wordmark header out — the tab bar stays — while `DeckStore.maximized`
  is set.
- `pick`/`pickResult` and `share` are the blob plane (below). Only the shell
  (main frame) may drive this handler at all — WKWebView injects the message
  handler into EVERY frame, so `DeckBridge` gates on `frameInfo.isMainFrame` or
  a sandboxed card could call the native surface directly instead of going
  through its port.

## Blobs: pick and share

`docs/modules/deck.md` §Blobs is the contract; the iOS-side shape:

- **`deck.pickBlob({accept})`** → `DeckStore.requestPick`. `accept` is a
  mime-glob list the SHELL has already normalized to one comma-joined string
  (`normalizeAccept`); `DeckStore.electPicker` re-parses it and returns a
  `PickerMode` — `.photos` for an all-image or absent list (the compat floor
  every pre-`accept` card depends on), `.files([UTType])` otherwise.
- **One mode, two presentations.** `DeckScreen` binds `.photosPicker` and
  `.fileImporter` to the same `pickerMode`; SwiftUI cannot bind two
  presentation modifiers to one Bool.
- **The two cancels are NOT shared.** `PhotosPicker` has no cancel callback, so
  `photosPickerDismissed` infers one from "dismissed with nothing chosen",
  deferred a runloop turn so a same-tick selection wins. `.fileImporter` has
  `onCancellation` → `filePickCancelled`, no inference. Every request settles
  exactly once through `settlePick`, whose id guard keeps a `busy` rejection
  (a *different* id) from freeing the pick it was rejected against.
- **The file leg streams.** It holds the URL's security-scoped access open for
  the whole upload (`ScopedFile`, RAII — an iCloud item reads as empty without
  it), sizes the file off disk, and calls `deckBlobUploadFile(path:…)` so a
  100 MiB pick never enters memory. The photo leg still uploads bytes.
- **`cardId` rides every upload** so the gateway stamps the blob
  `deck:<card_id>` — without it the blob is an immortal `device:*` orphan, since
  card purge is the only sweeper.
- **`deck.shareBlob`** → `requestShare`: fetch cache-first, materialize under a
  real filename, present the share sheet via `DeckStore.shareItem`.

## Card size

Card size is per-card adaptive. The manifest declares:

- `sizes` — the grid sizes the card implements; the ⤢ cycle stays in that set.
- `maximize` — a full-screen `deck.size === "max"` layout via a ⛶ button.

Both ride the `DeckCardInfo` / `DeckStore.Card` model, and the shell drives the
expand with the drag engine's fixed-tile trick (**zero iframe reload**).

## Live resize and safe-area insets (iPhone Duo)

The deck webview is full-bleed (`DeckScreen` ignores the safe area), and on
iPhone Duo the app is resized LIVE between the outer display (~466pt, compact)
and the inner one (~951pt, regular), while Split View on the inner display and
landscape on the outer one give **unequal left/right insets**. So the shell
never assumes left == right, and nothing it lays out may freeze viewport px:

- **Grid gutters** are `max(var(--deck-gutter), env(safe-area-inset-<side>))`
  per side.
- **The grid caps at `--deck-grid-max-width` and centers** on a wide window.
  Card fragments are written for phone-width tiles and adapt only to the
  semantic `deck.size`, so a stretched ~460pt Small tile is the failure. It
  stays 2 columns: the column count is what defines Wide/Large for every
  installed card, which is a design change, not a layout fix.
- **A maximized card's expanded rect is CSS** (`.deck-card.maximized.deck-max-full`),
  not inline px, so it follows a resize without touching the iframe node
  (moving it reloads the card). The side insets are padding on the paper
  tile: the sandboxed card can't read the shell's insets, and the tile still
  covers the grid behind the inset strips. The vertical clearances stay the
  card base's `--deck-header-clearance` / `--deck-tab-bar-clearance`; the
  tab bar's placement on Duo is unverified until there is a simulator.
- **A width change aborts an in-flight drag** (`onViewportResize`): the
  lifted tile settles at once into the slot its placeholder already showed,
  and the release point — captured against the old columns — is never used
  to re-resolve one. Height-only changes (keyboard, bars) don't abort.
