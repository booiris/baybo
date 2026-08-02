import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { postHtmlPreviewMaximized } from "./bridge";
import {
  HTML_PREVIEW_COLLAPSE_EVENT,
  htmlPreviewUrl,
} from "./htmlPreviewProtocol";

export function HtmlPreview({ blobId }: { blobId: string }) {
  const { t } = useTranslation();
  const [reload, setReload] = useState(0);
  const [loadedSrc, setLoadedSrc] = useState<string | null>(null);
  const [maximized, setMaximizedState] = useState(false);
  const maximizedRef = useRef(false);
  const src = htmlPreviewUrl(blobId, reload);

  const setMaximized = useCallback((next: boolean) => {
    if (maximizedRef.current === next) return;
    if (next) {
      window.dispatchEvent(new Event(HTML_PREVIEW_COLLAPSE_EVENT));
    }
    maximizedRef.current = next;
    setMaximizedState(next);
    document.documentElement.classList.toggle("html-preview-maximized", next);
    postHtmlPreviewMaximized(next);
  }, []);

  useEffect(() => {
    const collapse = () => setMaximized(false);
    window.addEventListener(HTML_PREVIEW_COLLAPSE_EVENT, collapse);
    return () => {
      window.removeEventListener(HTML_PREVIEW_COLLAPSE_EVENT, collapse);
      if (!maximizedRef.current) return;
      maximizedRef.current = false;
      document.documentElement.classList.remove("html-preview-maximized");
      postHtmlPreviewMaximized(false);
    };
  }, [setMaximized]);

  return (
    <span
      className={maximized ? "html-preview is-maximized" : "html-preview"}
      role="group"
      aria-label={t("chat.htmlPreview")}
    >
      <span className="html-preview-toolbar">
        <span className="html-preview-label">{t("chat.htmlPreview")}</span>
        <span className="html-preview-actions">
          <button
            type="button"
            className="html-preview-action"
            aria-label={t("chat.reloadHtmlPreview")}
            onClick={() => {
              setLoadedSrc(null);
              setReload((value) => value + 1);
            }}
          >
            <span aria-hidden="true">↻</span>
          </button>
          <button
            type="button"
            className="html-preview-action"
            aria-label={
              maximized
                ? t("chat.closeHtmlPreview")
                : t("chat.expandHtmlPreview")
            }
            onClick={() => setMaximized(!maximized)}
          >
            <span aria-hidden="true">{maximized ? "×" : "⛶"}</span>
          </button>
        </span>
      </span>
      {loadedSrc !== src && (
        <span className="html-preview-loading" role="status">
          <span className="html-preview-spinner" aria-hidden="true" />
          {t("chat.loadingHtmlPreview")}
        </span>
      )}
      <iframe
        className={loadedSrc === src ? "html-preview-frame is-loaded" : "html-preview-frame"}
        src={src}
        title={t("chat.htmlPreviewFrameTitle")}
        sandbox="allow-scripts"
        referrerPolicy="no-referrer"
        loading="lazy"
        onLoad={() => setLoadedSrc(src)}
      />
    </span>
  );
}

export function InvalidHtmlPreview() {
  const { t } = useTranslation();
  return (
    <span className="html-preview html-preview-invalid" role="alert">
      {t("chat.invalidHtmlPreview")}
    </span>
  );
}
