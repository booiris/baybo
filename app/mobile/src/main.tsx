import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
// Bundled, self-hosted fonts (no CDN — the app keeps its offline/no-needless-
// network posture). Space Mono carries the monospace chrome; Inter is the sans
// used for chat message bodies. Imported before styles.css so the @font-face
// families exist when the theme references them.
import "@fontsource/space-mono/400.css";
import "@fontsource/space-mono/700.css";
import "@fontsource-variable/inter";
// Initializes i18next (en + zh, with localStorage/navigator language detection)
// as a side effect — must run before <App> so the first render is localized.
import "./i18n";
import "./styles.css";
import { installKeyboardInsetTracking } from "./viewport";

// Track the keyboard (html.kb-open) so the chat composer drops its now-redundant
// home-indicator inset and sits just above the keyboard instead of a band above it.
installKeyboardInsetTracking();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
