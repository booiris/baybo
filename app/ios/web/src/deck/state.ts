// Pure deck-shell state: card list + latest-snapshot cells, the seq
// accept rule, layout math, and srcdoc composition. Everything here is
// DOM-free so vitest covers it (the repo's tier-2 discipline: reducers,
// not mounts).

export type DeckSize = "small" | "wide" | "large";

/// The runtime size a card is currently rendered at. A grid `DeckSize`, or
/// `"max"` while the card is maximized full-screen — the value pushed to the
/// card over its port and reported by `deck.size` / `deck.onSizeChange`.
export type RenderSize = DeckSize | "max";

/// Canonical size order (small < wide < large) so the ⤢ cycle is stable
/// regardless of the order the card declared its `sizes` in.
const SIZE_ORDER: DeckSize[] = ["small", "wide", "large"];

function isDeckSize(s: unknown): s is DeckSize {
  return s === "small" || s === "wide" || s === "large";
}

export type DeckCard = {
  cardId: string;
  title: string;
  position: number;
  size: DeckSize;
  /// The grid sizes the card implements (canonical order, always contains
  /// `size`, never empty). Normalized from the payload by `normalizeCard`.
  sizes: DeckSize[];
  /// Whether the card declares a maximized layout (the ⛶ affordance).
  maximize: boolean;
  enabled: boolean;
  quarantined: boolean;
  specHash: string;
  lastSeq: number;
};

/// Fill `sizes` / `maximize` for a card coming off the bridge: a pre-field
/// mirror or a legacy card omits them, so default `sizes` to `[size]` (a
/// single-size, never-adapted card) and `maximize` to false. Also drops any
/// bogus size token and guarantees `size ∈ sizes`.
export function normalizeCard(raw: DeckCard): DeckCard {
  const declared = Array.isArray(raw.sizes) ? raw.sizes.filter(isDeckSize) : [];
  const set = new Set<DeckSize>(declared.length > 0 ? declared : [raw.size]);
  set.add(raw.size);
  const sizes = SIZE_ORDER.filter((s) => set.has(s));
  return { ...raw, sizes, maximize: raw.maximize === true };
}

export type DeckSnapshot = {
  cardId: string;
  seq: number;
  payload: string;
  error?: string | null;
};

export type SnapshotCell = {
  seq: number;
  payload: string;
  error: string | null;
};

export type DeckState = {
  cards: DeckCard[];
  /// `| undefined` is the honest index type: a card can have no cached
  /// snapshot yet, and lookups must handle the miss.
  snaps: Record<string, SnapshotCell | undefined>;
};

export const EMPTY_STATE: DeckState = { cards: [], snaps: {} };

/// A refetch (`GET /v1/deck`, triggered by DeckChanged) REPLACES cached
/// snapshots + seqs unconditionally — the refetch is the authority.
export function replaceState(
  cards: DeckCard[],
  snapshots: DeckSnapshot[],
): DeckState {
  const snaps: Record<string, SnapshotCell> = {};
  for (const s of snapshots) {
    snaps[s.cardId] = { seq: s.seq, payload: s.payload, error: s.error ?? null };
  }
  return { cards: cards.map(normalizeCard).sort(byPosition), snaps };
}

function byPosition(a: DeckCard, b: DeckCard): number {
  const byPos = a.position - b.position;
  return byPos !== 0 ? byPos : a.cardId.localeCompare(b.cardId);
}

/// Live push rule: accept iff seq is strictly greater than the cached
/// seq for the card. Pushes for unknown cards are dropped (the
/// DeckChanged refetch will bring the card itself).
export function applyCardData(
  state: DeckState,
  cardId: string,
  seq: number,
  payload: string,
): { state: DeckState; changed: boolean } {
  if (!state.cards.some((c) => c.cardId === cardId)) {
    return { state, changed: false };
  }
  const cached = state.snaps[cardId];
  if (cached !== undefined && seq <= cached.seq) {
    return { state, changed: false };
  }
  return {
    state: {
      cards: state.cards,
      snaps: { ...state.snaps, [cardId]: { seq, payload, error: null } },
    },
    changed: true,
  };
}

/// Reorder to the DOM order SortableJS settled on: cards adopt
/// `orderedIds`' sequence (unknown ids ignored, missing cards keep their
/// relative order at the tail), then every position renumbers to its
/// index so the server layout matches what is shown.
export function reorderTo(cards: DeckCard[], orderedIds: string[]): DeckCard[] {
  const byId = new Map(cards.map((c) => [c.cardId, c]));
  const next: DeckCard[] = [];
  for (const id of orderedIds) {
    const card = byId.get(id);
    if (card) {
      next.push(card);
      byId.delete(id);
    }
  }
  for (const card of cards) {
    if (byId.has(card.cardId)) next.push(card);
  }
  return next.map((c, i) => ({ ...c, position: i }));
}

export function setCardSize(
  cards: DeckCard[],
  cardId: string,
  size: DeckSize,
): DeckCard[] {
  return cards.map((c) => (c.cardId === cardId ? { ...c, size } : c));
}

export type LayoutEntry = { cardId: string; position: number; size: DeckSize };

export function layoutEntries(cards: DeckCard[]): LayoutEntry[] {
  return cards.map((c, i) => ({ cardId: c.cardId, position: i, size: c.size }));
}

/// Next size in the card's own implemented set (canonical order, wrapping).
/// A single-size card has nothing to cycle to — the caller hides the control,
/// but this stays a no-op for safety. An off-set current value starts the
/// cycle at the set's first size.
export function cycleSize(size: DeckSize, sizes: DeckSize[]): DeckSize {
  const ordered = SIZE_ORDER.filter((s) => sizes.includes(s));
  if (ordered.length <= 1) return ordered[0] ?? size;
  const i = ordered.indexOf(size);
  return ordered[(i + 1) % ordered.length] ?? ordered[0];
}

/// The card iframe's CSP: no network of any kind, inline script/style +
/// data: images only. Belt to the sandbox attribute's braces — the
/// sandbox alone does NOT block a fire-and-forget fetch.
export const CARD_CSP =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:;";

/// Compose a card's srcdoc: CSP first, then the card-side SDK (so the
/// `deck` global exists before any card inline script runs), then the
/// agent-written fragment.
export function buildSrcdoc(
  cardHtml: string,
  sdkSource: string,
  baseCss: string,
): string {
  // `baseCss` (cardBase.css) is the injected design language: widget
  // behavior (no text selection / touch callout — WebKit would otherwise
  // claim a long-press for the selection UI before the SDK's edit-hold
  // detector sees it; no scrolling) plus the app's tokens and utility
  // classes. Injected BEFORE the fragment so the card's own <style>
  // overrides it in the cascade.
  return (
    `<!doctype html><html><head>` +
    `<meta http-equiv="Content-Security-Policy" content="${CARD_CSP}">` +
    `<meta name="viewport" content="width=device-width, initial-scale=1.0, user-scalable=no">` +
    `<style>${baseCss}</style>` +
    `</head><body>` +
    `<script>${sdkSource}</script>` +
    cardHtml +
    `</body></html>`
  );
}

/// Parse a snapshot payload cell into the value handed to the card.
/// Invalid JSON (should be impossible — the gateway stores JSON text)
/// degrades to null rather than wedging the card.
export function parsePayload(cell: SnapshotCell): unknown {
  if (cell.error !== null && cell.error !== "") return { error: cell.error };
  try {
    return JSON.parse(cell.payload) as unknown;
  } catch {
    return null;
  }
}
