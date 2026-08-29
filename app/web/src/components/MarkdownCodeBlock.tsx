import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
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

export type HighlightedCode = {
  html: string;
  language: string | null;
};

export function highlightCode(code: string, requestedLanguage?: string | null): HighlightedCode | null {
  const requested = requestedLanguage?.trim().toLowerCase() ?? '';
  if (PLAIN_LANGUAGES.has(requested)) return null;
  try {
    if (requested !== '') {
      if (hljs.getLanguage(requested) === undefined) return null;
      const result = hljs.highlight(code, { language: requested, ignoreIllegals: true });
      return { html: result.value, language: result.language ?? null };
    }
    const result = hljs.highlightAuto(code, AUTO_LANGUAGES);
    return result.language === undefined ? null : { html: result.value, language: result.language };
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
  const shown = code.endsWith('\n') ? code.slice(0, -1) : code;
  const highlighted = useMemo(() => highlightCode(shown, language), [shown, language]);
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
