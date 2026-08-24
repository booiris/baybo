import React from "react";
import ReactDOM from "react-dom/client";
// The same self-hosted faces and KaTeX css the transcript loads. Repeated here
// rather than shared through a module because these are Vite entry-level side
// effects: an import in a shared file would pull the css into whichever chunk
// happened to reach it first, and the two entries do not share a chunk.
import "@fontsource/space-mono/latin-400.css";
import "@fontsource/space-mono/latin-ext-400.css";
import "@fontsource/space-mono/latin-700.css";
import "@fontsource/space-mono/latin-ext-700.css";
import "@fontsource-variable/inter";
import "katex/dist/katex.min.css";
import i18n from "../i18n";
import "../styles.css";
import "./issue.css";
import { postIssueReady, subscribeIssue } from "./bridge";
import { IssuePage } from "./IssuePage";

// Language is native's to set, exactly as it is for the transcript.
subscribeIssue({
  init: (payload) => void i18n.changeLanguage(payload.language),
  deliver: () => undefined,
  bottomInset: () => undefined,
  language: (lang) => void i18n.changeLanguage(lang),
  setEditing: () => undefined,
  jumpToLatest: () => undefined,
});

const root = document.getElementById("issue-root");
if (!root) throw new Error("issue-root missing");

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <IssuePage />
  </React.StrictMode>,
);

// Native buffers everything until this lands — the transcript's ready contract.
postIssueReady();
