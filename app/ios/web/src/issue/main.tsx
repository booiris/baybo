import React, { useEffect, useState } from "react";
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
import {
  onIssueInit,
  onIssueLanguage,
  onIssuePresentation,
  postIssueReady,
  type IssuePresentation,
} from "./bridge";
import { IssuePage } from "./IssuePage";

// Language is native's to set, exactly as it is for the transcript — and on
// its OWN listener, never through `subscribeIssue`. That slot has one holder
// and it is the card: a stub parked there to catch `init` is handed the first
// `deliver` too, and a `deliver: () => undefined` swallows the card whole.
onIssueLanguage((lang) => void i18n.changeLanguage(lang));
onIssueInit((payload) => void i18n.changeLanguage(payload.language));

const root = document.getElementById("issue-root");
if (!root) throw new Error("issue-root missing");

function IssueRoot() {
  const [presentation, setPresentation] = useState<IssuePresentation | null>(null);
  useEffect(() => onIssuePresentation(setPresentation), []);
  if (presentation === null) return null;
  const { init, payload } = presentation;
  return (
    <IssuePage
      key={init.targetId}
      targetId={init.targetId}
      initialBottomInset={init.bottomInset}
      initialState={init.restoredState}
      initialPayload={payload}
    />
  );
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <IssueRoot />
  </React.StrictMode>,
);

// Native buffers everything until this lands — the transcript's ready contract.
postIssueReady();
