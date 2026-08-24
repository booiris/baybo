import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// base './': the bundle is loaded from a custom scheme inside the WKWebView,
// so every asset URL must stay relative to the entry html.
//
// Three entries, one dist: index.html (the transcript, TranscriptHost),
// deck.html (the deck shell, DeckHost) and issue.html (a project card,
// IssueHost). All three ride the same App/Resources/transcript copy + scheme
// handler — a second resource dir would buy nothing but a second copy step.
export default defineConfig({
  base: "./",
  plugins: [react()],
  build: {
    target: "es2021",
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("./index.html", import.meta.url)),
        deck: fileURLToPath(new URL("./deck.html", import.meta.url)),
        issue: fileURLToPath(new URL("./issue.html", import.meta.url)),
      },
    },
  },
});
