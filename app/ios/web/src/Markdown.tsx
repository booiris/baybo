import { memo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "./bridge";

// GFM only (tables, strikethrough, autolinks) — same plugin set as the web
// chat. No raw-HTML rendering (react-markdown default), so no sanitizer needed.
const REMARK_PLUGINS = [remarkGfm];

const COMPONENTS: Components = {
  // Every link hands off to native (system browser); an in-webview navigation
  // would replace the transcript page.
  a({ href, children }) {
    return (
      <a
        href={href}
        onClick={(e) => {
          e.preventDefault();
          if (href) openUrl(href);
        }}
      >
        {children}
      </a>
    );
  },
  // A wide table gets its OWN horizontal scroller (the reading band is narrow on
  // phone). Without the wrapper the table inherits the prose's `overflow-wrap:
  // anywhere` and shrinks its cells to fit instead of overflowing — so it never
  // scrolls, it just folds each cell into a tall column of broken words. The
  // wrapper is the scroll box; `white-space: nowrap` on the cells (styles.css)
  // keeps content on one line so the table grows past the band and scrolls.
  table({ children }) {
    return (
      <div className="md-table-wrap">
        <table>{children}</table>
      </div>
    );
  },
};

/// The assistant-prose renderer. Memoized: during a stream the parent
/// re-renders per animation frame, and without this every finalized message in
/// the log would re-parse its markdown on each tick. Statically imported (not
/// code-split): a lazy chunk left the first cold-start paint showing raw
/// markdown source until the chunk loaded, then reflowed to formatted prose —
/// a visible flash. Bundling it into the entry means the first paint is already
/// rendered.
export const MarkdownBody = memo(function MarkdownBody({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={REMARK_PLUGINS} components={COMPONENTS}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
