import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// base './': the bundle is loaded from a custom scheme inside the WKWebView,
// so every asset URL must stay relative to the entry html.
//
// Transcript, deck, and project-card entries share one embedded resource bundle.
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
