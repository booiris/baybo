import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { LanguageFn } from "highlight.js";
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import css from "highlight.js/lib/languages/css";
import diff from "highlight.js/lib/languages/diff";
import go from "highlight.js/lib/languages/go";
import java from "highlight.js/lib/languages/java";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import kotlin from "highlight.js/lib/languages/kotlin";
import markdown from "highlight.js/lib/languages/markdown";
import objectivec from "highlight.js/lib/languages/objectivec";
import php from "highlight.js/lib/languages/php";
import python from "highlight.js/lib/languages/python";
import ruby from "highlight.js/lib/languages/ruby";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import swift from "highlight.js/lib/languages/swift";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";
import { copyText } from "./bridge";

const COPIED_RESET_MS = 1200;

const LANGUAGES: Array<[string, LanguageFn]> = [
  ["bash", bash],
  ["c", c],
  ["cpp", cpp],
  ["csharp", csharp],
  ["css", css],
  ["diff", diff],
  ["go", go],
  ["java", java],
  ["javascript", javascript],
  ["json", json],
  ["kotlin", kotlin],
  ["markdown", markdown],
  ["objectivec", objectivec],
  ["php", php],
  ["python", python],
  ["ruby", ruby],
  ["rust", rust],
  ["sql", sql],
  ["swift", swift],
  ["typescript", typescript],
  ["xml", xml],
  ["yaml", yaml],
];

for (const [name, language] of LANGUAGES) {
  if (hljs.getLanguage(name) === undefined) hljs.registerLanguage(name, language);
}

const AUTO_LANGUAGES = LANGUAGES.map(([name]) => name);
const PLAIN_LANGUAGES = new Set(["text", "plain", "plaintext", "txt", "console", "output"]);

function highlightCode(
  code: string,
  requestedLanguage?: string | null,
): { html: string; language: string | null } | null {
  const requested = requestedLanguage?.trim().toLowerCase() ?? "";
  if (PLAIN_LANGUAGES.has(requested)) return null;
  try {
    if (requested !== "") {
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

function CopyGlyph({ copied }: { copied: boolean }) {
  return copied ? (
    <svg viewBox="0 0 20 20" aria-hidden="true">
      <path d="m4.5 10.5 3.2 3.2 7.8-8" />
    </svg>
  ) : (
    <svg viewBox="0 0 20 20" aria-hidden="true">
      <rect x="6.5" y="6.5" width="9" height="9" rx="1.5" />
      <path d="M4.5 13.5h-1v-10h10v1" />
    </svg>
  );
}

export function MarkdownCodeBlock({ code, language }: { code: string; language?: string | null }) {
  const { t } = useTranslation();
  const shown = code.endsWith("\n") ? code.slice(0, -1) : code;
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
    copyText(shown);
    setCopiedCode(shown);
    if (resetTimer.current !== null) window.clearTimeout(resetTimer.current);
    resetTimer.current = window.setTimeout(() => setCopiedCode(null), COPIED_RESET_MS);
  }, [shown]);

  const label = language?.trim() ?? "";
  return (
    <div className="md-code-block">
      {label === "" ? null : <span className="md-code-language">{label}</span>}
      <button
        type="button"
        className="md-code-copy"
        onClick={handleCopy}
        title={copied ? t("chat.copied") : t("chat.copyCode")}
        aria-label={copied ? t("chat.copied") : t("chat.copyCode")}
      >
        <CopyGlyph copied={copied} />
      </button>
      <pre>
        {highlighted === null ? (
          <code>{shown}</code>
        ) : (
          // highlight.js escapes source text before adding its own span markup.
          <code
            className={`hljs language-${highlighted.language ?? "plain"}`}
            dangerouslySetInnerHTML={{ __html: highlighted.html }}
          />
        )}
      </pre>
    </div>
  );
}
