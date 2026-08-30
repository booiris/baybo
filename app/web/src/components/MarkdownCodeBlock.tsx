import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { RiCheckLine, RiFileCopyLine } from 'react-icons/ri';
import type { LanguageFn } from 'highlight.js';
import hljs from 'highlight.js/lib/core';
import bash from 'highlight.js/lib/languages/bash';
import c from 'highlight.js/lib/languages/c';
import cpp from 'highlight.js/lib/languages/cpp';
import csharp from 'highlight.js/lib/languages/csharp';
import css from 'highlight.js/lib/languages/css';
import diff from 'highlight.js/lib/languages/diff';
import go from 'highlight.js/lib/languages/go';
import java from 'highlight.js/lib/languages/java';
import javascript from 'highlight.js/lib/languages/javascript';
import json from 'highlight.js/lib/languages/json';
import kotlin from 'highlight.js/lib/languages/kotlin';
import markdown from 'highlight.js/lib/languages/markdown';
import objectivec from 'highlight.js/lib/languages/objectivec';
import php from 'highlight.js/lib/languages/php';
import python from 'highlight.js/lib/languages/python';
import ruby from 'highlight.js/lib/languages/ruby';
import rust from 'highlight.js/lib/languages/rust';
import sql from 'highlight.js/lib/languages/sql';
import swift from 'highlight.js/lib/languages/swift';
import typescript from 'highlight.js/lib/languages/typescript';
import xml from 'highlight.js/lib/languages/xml';
import yaml from 'highlight.js/lib/languages/yaml';

export const CODE_COPY_FEEDBACK_MS = 1200;

const LANGUAGES: Array<[string, LanguageFn]> = [
  ['bash', bash],
  ['c', c],
  ['cpp', cpp],
  ['csharp', csharp],
  ['css', css],
  ['diff', diff],
  ['go', go],
  ['java', java],
  ['javascript', javascript],
  ['json', json],
  ['kotlin', kotlin],
  ['markdown', markdown],
  ['objectivec', objectivec],
  ['php', php],
  ['python', python],
  ['ruby', ruby],
  ['rust', rust],
  ['sql', sql],
  ['swift', swift],
  ['typescript', typescript],
  ['xml', xml],
  ['yaml', yaml],
];

for (const [name, language] of LANGUAGES) {
  if (hljs.getLanguage(name) === undefined) hljs.registerLanguage(name, language);
}

const AUTO_LANGUAGES = LANGUAGES.map(([name]) => name);
const PLAIN_LANGUAGES = new Set(['text', 'plain', 'plaintext', 'txt', 'console', 'output']);

/** True while the surrounding `MarkdownBody`'s text is still streaming in.
 *  A growing block's `shown` changes every pacer tick, so the highlight memo
 *  can never hold — and an unclosed fence swallows the whole remaining stream,
 *  so "the growing block" is routinely the entire tail of the answer. On iOS
 *  the identical construction re-highlighted per tick until WebContent hit the
 *  2.2GB per-process jetsam limit (the white-flash loop); a desktop
 *  tab survives but stalls for seconds and churns GBs of GC garbage. While
 *  streaming, the block renders plain — React then patches one text node per
 *  tick instead of swapping the whole highlighted subtree — and the settle
 *  render colors it once. */
export const MarkdownStreamingContext = createContext(false);

/** Hard ceiling on what gets highlighted at all. `highlightAuto` allocates
 *  ~100MB of transient garbage per call at 64KB input (measured against this
 *  hljs build), and `highlightCode` also runs per keystroke for every block in
 *  the projects card editor (`markdownCodePlugin`). Past the cap the block is
 *  plain text — readable, just uncolored. */
const HIGHLIGHT_MAX_CHARS = 32 * 1024;

/** Unlabeled fences detect their language on a prefix, then highlight the full
 *  text with the single winning grammar — auto-detect over all 22 grammars
 *  holds every candidate's full result simultaneously and is ~20× slower. */
const DETECT_SLICE_CHARS = 4 * 1024;

export type HighlightedCode = {
  html: string;
  language: string | null;
};

export function highlightCode(code: string, requestedLanguage?: string | null): HighlightedCode | null {
  const requested = requestedLanguage?.trim().toLowerCase() ?? '';
  if (PLAIN_LANGUAGES.has(requested)) return null;
  if (code.length > HIGHLIGHT_MAX_CHARS) return null;
  try {
    if (requested !== '') {
      if (hljs.getLanguage(requested) === undefined) return null;
      const result = hljs.highlight(code, { language: requested, ignoreIllegals: true });
      return { html: result.value, language: result.language ?? null };
    }
    const detected = hljs.highlightAuto(code.slice(0, DETECT_SLICE_CHARS), AUTO_LANGUAGES).language;
    if (detected === undefined) return null;
    const result = hljs.highlight(code, { language: detected, ignoreIllegals: true });
    return { html: result.value, language: result.language ?? detected };
  } catch {
    return null;
  }
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const textarea = document.createElement('textarea');
    textarea.value = text;
    textarea.setAttribute('readonly', '');
    textarea.style.position = 'fixed';
    textarea.style.opacity = '0';
    document.body.append(textarea);
    textarea.select();
    const copied = typeof document.execCommand === 'function' && document.execCommand('copy');
    textarea.remove();
    return copied;
  }
}

export function MarkdownCodeBlock({ code, language }: { code: string; language?: string | null }) {
  const streamingText = useContext(MarkdownStreamingContext);
  const shown = code.endsWith('\n') ? code.slice(0, -1) : code;
  const highlighted = useMemo(
    () => (streamingText ? null : highlightCode(shown, language)),
    [shown, language, streamingText],
  );
  const [copiedCode, setCopiedCode] = useState<string | null>(null);
  const resetTimer = useRef<number | null>(null);
  const copied = copiedCode === shown;

  useEffect(
    () => () => {
      if (resetTimer.current !== null) window.clearTimeout(resetTimer.current);
    },
    [],
  );

  const handleCopy = useCallback(() => {
    void copyText(shown).then((didCopy) => {
      if (!didCopy) return;
      setCopiedCode(shown);
      if (resetTimer.current !== null) window.clearTimeout(resetTimer.current);
      resetTimer.current = window.setTimeout(() => setCopiedCode(null), CODE_COPY_FEEDBACK_MS);
    });
  }, [shown]);

  const label = language?.trim() ?? '';
  return (
    <div className="md-code-block group/code">
      {label === '' ? null : <span className="md-code-language">{label}</span>}
      <button
        type="button"
        className="md-code-copy"
        onClick={handleCopy}
        title={copied ? 'Copied' : 'Copy code'}
        aria-label={copied ? 'Copied' : 'Copy code'}
      >
        {copied ? <RiCheckLine aria-hidden /> : <RiFileCopyLine aria-hidden />}
      </button>
      <pre>
        {highlighted === null ? (
          <code>{shown}</code>
        ) : (
          // highlight.js escapes source text before adding its own span markup.
          <code
            className={`hljs language-${highlighted.language ?? 'plain'}`}
            dangerouslySetInnerHTML={{ __html: highlighted.html }}
          />
        )}
      </pre>
    </div>
  );
}
