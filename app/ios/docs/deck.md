# Deck (iOS shell)

*`docs/modules/deck.md` is the source of truth for the Deck design; this document covers only the iOS half — `app/ios/App/Core/DeckStore.swift`, `app/ios/App/Web/DeckBridge.swift`, and the `app/ios/web/src/deck/` shell.*

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

## Bridge

`app/ios/App/Web/DeckBridge.swift` ⇄ `app/ios/web/src/deck/bridge.ts`.

native→web:

```
init/deckState/cardData/bundle/callResult/setEditMode/setLanguage
```

web→native:

```
ready/refetch/requestBundle/call/layout/cardAction/editMode/quickSetup/maximize/log
```

- `quickSetup` is the empty-board CTA: native opens a fresh chat and auto-sends
  a `/deck` request, via `AppStore.startCardDraft`.
- `maximize` reports a card entered/left its full-screen layout so `DeckScreen`
  fades the wordmark header out — the tab bar stays — while `DeckStore.maximized`
  is set.

## Card size

Card size is per-card adaptive. The manifest declares:

- `sizes` — the grid sizes the card implements; the ⤢ cycle stays in that set.
- `maximize` — a full-screen `deck.size === "max"` layout via a ⛶ button.

Both ride the `DeckCardInfo` / `DeckStore.Card` model, and the shell drives the
expand with the drag engine's fixed-tile trick (**zero iframe reload**).
