// Ported verbatim from `app/ios/web/src/mathDelimiters.ts` (a separate pnpm
// workspace root, so the module can't simply be shared). The two clients must
// render the same message identically — `diff` the pair before changing either.
//
// Normalizes the LaTeX an assistant emits into the exact syntax `remark-math`
// needs, before the markdown is parsed. Over the non-code parts of the source
// (code is masked out first, by a linear scan):
//
//  1. Dollar guard. `remark-math` has no "a closing $ is not before a digit"
//     rule, so it pairs `$5 and $3` into inline math and turns a price sentence
//     into garbage. `guardDollars` keeps only `$...$` spans that are valid inline
//     math under pandoc's rules (opener not before whitespace; closer not after
//     whitespace and not before a digit) and escapes every other `$`. This keeps
//     real math — including `$3.14$`, `$2 + 2 = 4$`, `$5 = x$` — while making
//     prose money (`$5 and $3`, `$12.50`) literal. The distinction is whether a
//     valid CLOSER exists, which no per-character heuristic can decide.
//  2/3. Delimiter style. Models also use the AMS/MathJax form (`\(...\)` inline,
//     `\[...\]` display); `remark-math` only tokenizes dollars, and CommonMark
//     eats the backslashes before any remark plugin runs, so the AMS form is
//     rewritten to dollars in the raw source.
//  4. Display blocks. `remark-math` renders `$$...$$` as a centered display block
//     only when the `$$` sit on their own lines; a whole-line column-0 `$$...$$`
//     is promoted, but one inside a list item / table cell / blockquote is left
//     inline so the enclosing block stays intact.

const FENCE = /^[ \t]*(`{3,}|~{3,})/;
const FENCE_CLOSE = /^[ \t]*(`{3,}|~{3,})[ \t]*$/;
const INLINE_CODE = /`[^`\n]+`/g;
// Capped bodies: an unclosed `\[` / `\(` otherwise scans to end-of-input, which
// is quadratic across many openers (and this runs per streaming frame). Real
// math bodies are far shorter than the cap.
const DISPLAY_BRACKET = /\\\[([\s\S]{1,2000}?)\\\]/g;
const INLINE_PAREN = /\\\(([\s\S]{1,2000}?)\\\)/g;
const DISPLAY_LINE = /^\$\$([^\n]+?)\$\$[ \t]*$/gm;
// Longest inline `$...$` span the guard will pair; past it, treat the `$` as
// literal. Bounds the closer scan (which runs per `$`, per streaming frame) —
// real inline math is far shorter; anything longer belongs in a `$$` block.
const MAX_INLINE_MATH = 256;
// NUL / SOH cannot occur in transcript text, so they are collision-free mask
// sentinels bracketing the placeholder index (built at runtime to keep the
// literal control bytes out of the source).
const MASK_OPEN = String.fromCharCode(0);
const MASK_CLOSE = String.fromCharCode(1);
const UNMASK = new RegExp(`${MASK_OPEN}(\\d+)${MASK_CLOSE}`, "g");

const isWs = (ch: string): boolean =>
  ch === " " || ch === "\t" || ch === "\n" || ch === "\r" || ch === "\f" || ch === "\v";
const isDigit = (ch: string): boolean => ch >= "0" && ch <= "9";
const isAlpha = (ch: string): boolean =>
  (ch >= "a" && ch <= "z") || (ch >= "A" && ch <= "Z");

// Emit a `$$...$$` display span verbatim (its body is math; the block promotion
// handles it later). Returns the index just past it, or -1 if `i` isn't a `$$`.
function passDisplay(src: string, i: number, emit: (s: string) => void): number {
  if (src.charAt(i) !== "$" || src.charAt(i + 1) !== "$") return -1;
  const close = src.indexOf("$$", i + 2);
  if (close !== -1) {
    emit(src.slice(i, close + 2));
    return close + 2;
  }
  emit("$$");
  return i + 2;
}

// Escapes every `$` that is not part of a valid inline math span, so remark-math
// (which has no "a closing $ is not before a digit" rule) cannot pair prose
// money — `$5 and $3`, `$12.50` — into math. Two passes, because money always
// starts `$<digit>`:
//   1. Mask the unambiguous LETTER/command-led spans (`$<non-digit>...$`) first,
//      so a later `$5` cannot pair ACROSS a real `$x$` span (the money-before-
//      math trap).
//   2. Keep a DIGIT-led span only when its FIRST following `$` is a valid closer
//      (not before a digit); escape every other `$`. This keeps `$3.14$`,
//      `$2 + 2 = 4$`, `$5 = x$` while making money literal.
function guardDollars(src: string, mask: (s: string) => string): string {
  // Pass 1.
  let out = "";
  let i = 0;
  let n = src.length;
  const emit = (s: string): void => {
    out += s;
  };
  while (i < n) {
    const c = src.charAt(i);
    if (c !== "$") {
      out += c;
      i++;
      continue;
    }
    const past = passDisplay(src, i, emit);
    if (past !== -1) {
      i = past;
      continue;
    }
    const next = src.charAt(i + 1);
    if (next !== "" && !isWs(next) && !isDigit(next)) {
      const closer = findCloser(src, i, n);
      if (closer !== -1) {
        out += mask(src.slice(i, closer + 1));
        i = closer + 1;
        continue;
      }
    }
    out += c;
    i++;
  }

  // Pass 2.
  const pass1 = out;
  out = "";
  i = 0;
  n = pass1.length;
  while (i < n) {
    const c = pass1.charAt(i);
    if (c !== "$") {
      out += c;
      i++;
      continue;
    }
    const past = passDisplay(pass1, i, emit);
    if (past !== -1) {
      i = past;
      continue;
    }
    if (isDigit(pass1.charAt(i + 1))) {
      const k = firstDollar(pass1, i, n);
      // A real closer for a digit-led span has math content immediately before it
      // (not whitespace) and does not itself open another token (not followed by a
      // letter or digit). This rejects a stray lone `$` ("the $ means") or an
      // unclosed `$var` ("$5 to $N") that `firstDollar` would otherwise accept.
      const prev = pass1.charAt(k - 1);
      const after = pass1.charAt(k + 1);
      if (k !== -1 && prev !== "$" && !isWs(prev) && !isDigit(after) && !isAlpha(after)) {
        out += pass1.slice(i, k + 1);
        i = k + 1;
        continue;
      }
    }
    out += "\\$";
    i++;
  }
  return out;
}

// First valid closer for a letter-led opener at `i`: a `$` not part of `$$` and
// not before a digit, within the span cap, not crossing a blank line.
function findCloser(src: string, i: number, n: number): number {
  const limit = Math.min(n, i + 1 + MAX_INLINE_MATH);
  for (let j = i + 1; j < limit; j++) {
    if (src.charAt(j) === "\n" && src.charAt(j + 1) === "\n") return -1;
    if (
      src.charAt(j) === "$" &&
      src.charAt(j - 1) !== "$" &&
      src.charAt(j + 1) !== "$" &&
      !isDigit(src.charAt(j + 1))
    ) {
      return j;
    }
  }
  return -1;
}

// The FIRST `$` after a digit-led opener at `i` (money must pair tightly — it may
// not scan past an intervening `$`), within the span cap, not crossing a blank line.
function firstDollar(src: string, i: number, n: number): number {
  const limit = Math.min(n, i + 1 + MAX_INLINE_MATH);
  for (let j = i + 1; j < limit; j++) {
    if (src.charAt(j) === "\n" && src.charAt(j + 1) === "\n") return -1;
    if (src.charAt(j) === "$") return j;
  }
  return -1;
}

export function normalizeMath(src: string): string {
  // Fast path: nothing to rewrite (the overwhelming majority of messages). This
  // runs per streaming frame.
  if (!src.includes("$") && !src.includes("\\(") && !src.includes("\\[")) return src;

  const masked: string[] = [];
  const mask = (s: string): string => {
    masked.push(s);
    return `${MASK_OPEN}${masked.length - 1}${MASK_CLOSE}`;
  };

  // 1. Mask code, line by line (linear; no catastrophic backtracking). A fenced
  //    block masks every line until its matching closer, or end-of-input if it
  //    is never closed (a malformed message, or the mid-stream state before the
  //    closing fence arrives). The fence indent is unbounded so a fence nested
  //    inside a list (indented past 3 spaces) is still recognised.
  const lines = src.split("\n");
  let fence: string | null = null;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (fence !== null) {
      const close = FENCE_CLOSE.exec(line);
      if (close !== null && close[1].charAt(0) === fence.charAt(0) && close[1].length >= fence.length) {
        fence = null;
      }
      lines[i] = mask(line);
      continue;
    }
    const open = FENCE.exec(line);
    if (open !== null) {
      fence = open[1];
      lines[i] = mask(line);
      continue;
    }
    lines[i] = line.replace(INLINE_CODE, mask);
  }
  let work = lines.join("\n");

  // 2. Escape non-math dollars, then 3. AMS -> dollar, then 4. own-line display.
  work = guardDollars(work, mask);
  // The closer check skips the scan entirely when a `\[` / `\(` has no matching
  // closer anywhere (a stream of unclosed openers, the reported quadratic case).
  if (work.includes("\\]")) work = work.replace(DISPLAY_BRACKET, (_m, body: string) => `$$${body}$$`);
  if (work.includes("\\)")) work = work.replace(INLINE_PAREN, (_m, body: string) => `$${body}$`);
  work = work.replace(DISPLAY_LINE, (_m, body: string) => `$$\n${body.trim()}\n$$`);

  // A digit run bracketed by literal NUL/SOH in the SOURCE looks exactly like a
  // placeholder we minted, and an out-of-range index would otherwise splice the
  // string "undefined" over the author's characters. Hand back the match.
  return work.replace(UNMASK, (m, i: string) => masked[Number(i)] ?? m);
}
