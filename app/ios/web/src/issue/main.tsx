import React, { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
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

// Language uses its own listener: taking the issue subscription here would
// swallow a delivery that arrives before React mounts.
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

postIssueReady();
