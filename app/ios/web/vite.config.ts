import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// base './': the bundle is loaded from file:// inside the WKWebView, so every
// asset URL must stay relative to index.html.
export default defineConfig({
  base: "./",
  plugins: [react()],
  build: {
    target: "es2021",
  },
});
