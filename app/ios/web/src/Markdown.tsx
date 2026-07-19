import { memo, useCallback, useEffect, useRef, type ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "./bridge";

// GFM only (tables, strikethrough, autolinks) — same plugin set as the web
// chat. No raw-HTML rendering (react-markdown default), so no sanitizer needed.
const REMARK_PLUGINS = [remarkGfm];

// Floor on the indicator thumb so a very wide table (its window a tiny fraction
// of the content) still shows a grabbable-looking bar rather than an invisible
// sliver. The band is always far wider than this, so the clamp never inverts
// the travel math.
const MIN_THUMB_PX = 28;

// A wide table scrolls sideways inside its own band (the reading band is narrow
// on phone — see the `.md-table-*` rules in styles.css). But iOS WKWebView only
// flashes its overlay scrollbar DURING an active scroll and ignores
// `::-webkit-scrollbar`, so an overflowing table just looks static — nothing
// tells the reader it can be dragged. This draws our own thin indicator under
// the table: always visible while the content overflows, hidden when it already
// fits, its thumb mirroring the scroll window.
function TableBlock({ children }: { children?: ReactNode }) {
  const scrollerRef = useRef<HTMLDivElement | null>(null);
  const barRef = useRef<HTMLDivElement | null>(null);
  const thumbRef = useRef<HTMLDivElement | null>(null);

  const sync = useCallback(() => {
    const scroller = scrollerRef.current;
    const bar = barRef.current;
    const thumb = thumbRef.current;
    if (scroller === null || bar === null || thumb === null) return;
    const overflow = scroller.scrollWidth - scroller.clientWidth;
    // 1px slack: sub-pixel rounding can report a hairline of "overflow" on a
    // table that visually fits — don't hint a scroll to nowhere.
    if (overflow <= 1) {
      bar.classList.remove("scrollable");
      return;
    }
    bar.classList.add("scrollable");
    // The bar is a sibling of the scroller under the same block, so its width
    // equals the scroller's client (visible) width — the track. The thumb is
    // that visible window as a fraction of the whole content.
    const track = scroller.clientWidth;
    const thumbW = Math.max((track / scroller.scrollWidth) * track, MIN_THUMB_PX);
    // Keep the thumb inside its track. WebKit clamps a nested scroller's
    // reported scrollLeft to [0, overflow], so this is belt-and-suspenders —
    // but it also pins the thumb if a momentum bounce ever leaked an
    // out-of-range offset, and neutralizes the (unreachable) narrow-band case
    // where the MIN_THUMB_PX floor would exceed the track.
    const maxX = Math.max(track - thumbW, 0);
    const x = Math.min(Math.max((scroller.scrollLeft / overflow) * maxX, 0), maxX);
    const width = `${thumbW}px`;
    // Width only changes on resize; skip the write on the scroll hot path so
    // scrolling touches transform alone (compositor, no layout).
    if (thumb.style.width !== width) thumb.style.width = width;
    thumb.style.transform = `translateX(${x}px)`;
  }, []);

  useEffect(() => {
    const scroller = scrollerRef.current;
    if (scroller === null) return;
    sync();
    // The band width changes on rotate / keyboard inset, and the table's own
    // width grows as cells stream in — observe both so the bar appears the
    // moment the content first overflows, not only on the next user scroll.
    const ro = new ResizeObserver(sync);
    ro.observe(scroller);
    const table = scroller.firstElementChild;
    if (table !== null) ro.observe(table);
    return () => ro.disconnect();
  }, [sync]);

  return (
    <div className="md-table-wrap">
      <div className="md-table-scroller" ref={scrollerRef} onScroll={sync}>
        <table>{children}</table>
      </div>
      <div className="md-table-bar" ref={barRef} aria-hidden="true">
        <div className="md-table-bar-thumb" ref={thumbRef} />
      </div>
    </div>
  );
}

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
  // scroller is the scroll box; the cells cap their width and wrap inside it
  // (styles.css), so the table grows past the band and scrolls, and `TableBlock`
  // paints the indicator bar under it.
  table({ children }) {
    return <TableBlock>{children}</TableBlock>;
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
