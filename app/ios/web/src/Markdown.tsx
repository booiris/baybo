import { Component, memo, useCallback, useEffect, useRef, type ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import remarkBreaks from "remark-breaks";
import remarkCjkFriendly from "remark-cjk-friendly/parseOnly";
import remarkCjkFriendlyGfmStrikethrough from "remark-cjk-friendly-gfm-strikethrough/parseOnly";
import rehypeKatex from "rehype-katex";
import { openUrl } from "./bridge";
import { HtmlPreview, InvalidHtmlPreview } from "./HtmlPreview";
import { htmlPreviewBlobId } from "./htmlPreviewProtocol";
import { normalizeMath } from "./mathDelimiters";

// GFM (tables, strikethrough, autolinks) + math. `remark-math` tokenizes the
// `$...$` / `$$...$$` spans into math nodes; the `\(...\)` / `\[...\]` form is
// rewritten to dollars in `normalizeMath` before parse. The source is never raw
// HTML (react-markdown default), so no sanitizer is needed. `baybo-html` is the
// sole explicit escape hatch: its code body is a validated blob id, and the
// bytes render in a separate opaque-origin/CSP iframe rather than entering this
// DOM.
//
// The two `cjk-friendly` extensions relax CommonMark's flanking rule, which
// otherwise refuses `**标题：**内容` — punctuation INSIDE the closing `**`, a CJK
// letter immediately outside it, makes the run neither left- nor right-flanking,
// so the delimiters render as literal asterisks. Chinese prose has no space to
// separate them with, and `**要点：**说明` is the single most common shape an
// assistant emits, so without this a large share of CJK bold silently degrades
// to raw `**`. They implement the CommonMark CJK-friendly-emphasis proposal.
// ORDER IS LOAD-BEARING: the strikethrough one REPLACES `remark-gfm`'s `~`
// construct rather than layering over it, so ahead of `remarkGfm` it is
// silently overwritten and only CJK `~~` stops pairing. Nor are they purely
// additive — an astral emoji immediately inside a delimiter now classifies as
// punctuation (the spec-correct read), so `**done 😀**text` no longer bolds.
// The CJK suite in `Markdown.test.tsx` pins both. `/parseOnly` because the
// default entry also ships the mdast serializer half, which nothing here runs.
const REMARK_PLUGINS = [
  remarkGfm,
  remarkMath,
  remarkCjkFriendly,
  remarkCjkFriendlyGfmStrikethrough,
];
// `breaks` opts a caller into `remark-breaks`, which turns every soft line break
// into a hard one. CommonMark folds a single newline into a space, which is
// right for an answer written in paragraphs but destroys a reasoning trace: the
// model writes it as short newline-separated lines, and they ran together into
// one block the moment the step started being parsed as markdown (it used to be
// `white-space: pre-wrap` raw text). Unlike the plugins above it installs no
// micromark extension — it is a transformer over the parsed mdast — so its
// position in the array carries no constraint. Deliberately NOT extended to the
// answer: `prose` steps are the answer's own bytes (see `WorkStepView`).
const REMARK_PLUGINS_BREAKS = [...REMARK_PLUGINS, remarkBreaks];
// `rehype-katex` renders the math nodes to KaTeX markup in the hast. It leaves
// `trust` off (so `\href`/`\includegraphics` stay disabled) and, on a malformed
// expression, renders the offending source in place rather than throwing — a
// bad `$...$` must never blank the whole message.
const REHYPE_PLUGINS = [rehypeKatex];

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
  code({ className, children }) {
    const blobId = htmlPreviewBlobId(className, String(children));
    if (blobId === "") return <InvalidHtmlPreview />;
    if (blobId !== null) return <HtmlPreview blobId={blobId} />;
    return <code className={className}>{children}</code>;
  },
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

/// react-markdown runs the whole parse inside `render`, so anything the pipeline
/// throws propagates to the React root and unmounts the ENTIRE transcript — a
/// blank chat, not a broken message. KaTeX really does throw: a lone low
/// surrogate inside a `$…$` span raises `RangeError: Invalid code point`, and a
/// slice at a UTF-16 code-unit boundary is exactly how one appears in transcript
/// text. Falling back to the raw source keeps the message readable.
///
/// `text` doubles as the reset key: the next chunk of a stream re-enters the
/// pipeline, so a transiently-malformed prefix recovers on its own rather than
/// pinning the row to plain text for the rest of the session.
class MarkdownFallback extends Component<
  { text: string; children: ReactNode },
  { failed: boolean; forText: string }
> {
  state = { failed: false, forText: this.props.text };

  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }

  static getDerivedStateFromProps(
    props: { text: string },
    state: { forText: string },
  ): { failed: boolean; forText: string } | null {
    return props.text === state.forText ? null : { failed: false, forText: props.text };
  }

  componentDidCatch(error: unknown): void {
    console.error("markdown render failed", error);
  }

  render(): ReactNode {
    if (this.state.failed) return <div className="md-failed">{this.props.text}</div>;
    return this.props.children;
  }
}

/// Its own component so `normalizeMath` runs as a DESCENDANT of the boundary: a
/// boundary catches what its children throw, and a call left in `MarkdownBody`'s
/// own render would sit in the boundary's PARENT — outside it — while walking
/// the same slice-damaged text KaTeX chokes on.
function MarkdownPipeline({ text, breaks }: { text: string; breaks: boolean }) {
  return (
    <ReactMarkdown
      remarkPlugins={breaks ? REMARK_PLUGINS_BREAKS : REMARK_PLUGINS}
      rehypePlugins={REHYPE_PLUGINS}
      components={COMPONENTS}
    >
      {normalizeMath(text)}
    </ReactMarkdown>
  );
}

/// The assistant-prose renderer. Memoized: during a stream the parent
/// re-renders per animation frame, and without this every finalized message in
/// the log would re-parse its markdown on each tick. Statically imported (not
/// code-split): a lazy chunk left the first cold-start paint showing raw
/// markdown source until the chunk loaded, then reflowed to formatted prose —
/// a visible flash. Bundling it into the entry means the first paint is already
/// rendered.
export const MarkdownBody = memo(function MarkdownBody({
  text,
  breaks = false,
}: {
  text: string;
  breaks?: boolean;
}) {
  return (
    <div className="md">
      <MarkdownFallback text={text}>
        <MarkdownPipeline text={text} breaks={breaks} />
      </MarkdownFallback>
    </div>
  );
});
