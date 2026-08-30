import React from "react";
import ReactDOM from "react-dom/client";
// Bundled, self-hosted fonts (no CDN — the app keeps its offline/no-needless-
// network posture). Imported before styles.css so the @font-face families exist
// when the theme references them.
import "@fontsource/space-mono/latin-400.css";
import "@fontsource/space-mono/latin-ext-400.css";
import "@fontsource/space-mono/latin-700.css";
import "@fontsource/space-mono/latin-ext-700.css";
import "@fontsource-variable/inter";
// KaTeX math styling + its self-hosted math fonts. Vite emits the referenced
// woff2 into the bundle and the transcript scheme handler serves them like every
// other asset — no CDN, so math renders offline. Imported before styles.css so
// our `.md .katex-*` overrides win the cascade.
import "katex/dist/katex.min.css";
import i18n from "./i18n";
import "./styles.css";
import { hasNativeBridge, onInit, onLanguage, postContentReady, postReady, postToNative } from "./bridge";
import { Transcript } from "./Transcript";

onLanguage((lang) => void i18n.changeLanguage(lang));

let root: ReactDOM.Root | null = null;
onInit((payload) => {
  void i18n.changeLanguage(payload.language);
  if (!root) {
    root = ReactDOM.createRoot(document.getElementById("root") as HTMLElement);
  }
  root.render(
    <React.StrictMode>
      <Transcript
        key={payload.sessionId}
        restored={payload.restoredState}
        initialConnEpoch={payload.connEpoch}
        expandUnansweredTail={payload.expandUnansweredTail}
      />
    </React.StrictMode>,
  );
  requestAnimationFrame(() => postContentReady());
});

postToNative({ type: "log", level: "info", message: "boot: main evaluated" });
postReady();

// Dev browser (pnpm dev, no WKWebView): native never replies to ready, so
// synthesize an empty init to render the bare transcript.
if (!hasNativeBridge) {
  window.baybo.init({
    language: "en",
    sessionId: "dev",
    restoredState: null,
    connEpoch: 0,
    expandUnansweredTail: false,
  });
}
