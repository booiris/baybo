import * as bridge from "./bridge";
import { DeckShell } from "./shell";
import "./deck.css";

const root = document.getElementById("deck-root");
if (!root) {
  throw new Error("deck-root missing");
}
const shell = new DeckShell(root);

window.deckShell = {
  init(payload) {
    shell.setLanguage(payload.lang);
    if (payload.editMode !== undefined) shell.setEditMode(payload.editMode);
    shell.applyDeckState(payload.cards, payload.snapshots);
  },
  deckState(payload) {
    shell.applyDeckState(payload.cards, payload.snapshots);
  },
  cardData(payload) {
    shell.applyCardData(payload.cardId, payload.seq, payload.payload);
  },
  bundle(payload) {
    shell.applyBundle(payload.cardId, payload.cardHtml);
  },
  callResult(payload) {
    shell.applyCallResult(payload.id, payload.ok, payload.value, payload.error);
  },
  pickResult(payload) {
    shell.applyPickResult(payload.id, payload.ok, payload.ref, payload.error);
  },
  setEditMode(active) {
    shell.setEditMode(active);
  },
  setTopInset(px) {
    // Same shape as the transcript bridge's: a CSS variable the stylesheet
    // already falls back from, not a shell method — nothing here needs to
    // re-render to pick it up.
    document.documentElement.style.setProperty("--native-top-inset", `${px}px`);
  },
  setLanguage(lang) {
    shell.setLanguage(lang);
  },
  restoreMaximized() {
    shell.restoreMaximizedCard();
  },
};

// Native buffers deckShell calls until this lands (the transcript
// bridge's ready contract).
bridge.postReady();
