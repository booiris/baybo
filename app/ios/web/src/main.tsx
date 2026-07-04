import React from "react";
import ReactDOM from "react-dom/client";
// Bundled, self-hosted fonts (no CDN — the app keeps its offline/no-needless-
// network posture). Space Mono carries the monospace chrome; Inter is the sans
// used for chat message bodies. Imported before styles.css so the @font-face
// families exist when the theme references them.
import "@fontsource/space-mono/400.css";
import "@fontsource/space-mono/700.css";
import "@fontsource-variable/inter";
import i18n from "./i18n";
import "./styles.css";
import { hasNativeBridge, onInit, onLanguage, postContentReady, postReady } from "./bridge";
import { Transcript } from "./Transcript";

onLanguage((lang) => void i18n.changeLanguage(lang));

let started = false;
onInit((payload) => {
  if (started) return;
  started = true;
  // Apply the language before the first render so the initial paint is already
  // localized (resources are bundled, so this settles in the same tick).
  void i18n
    .changeLanguage(payload.language)
    .catch(() => {})
    .then(() => {
      ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
        <React.StrictMode>
          <Transcript restored={payload.restoredState} initialConnEpoch={payload.connEpoch} />
        </React.StrictMode>,
      );
      // Two frames after the first render commits the transcript has painted —
      // tell native so it fades the webview in instead of popping the content
      // in as the chat screen slides on.
      requestAnimationFrame(() => requestAnimationFrame(() => postContentReady()));
    });
});

postReady();

// Dev browser (pnpm dev, no WKWebView): native never replies to ready, so
// synthesize an empty init to render the bare transcript.
if (!hasNativeBridge) {
  window.baybo.init({ language: "en", sessionId: "dev", restoredState: null, connEpoch: 0 });
}
