/**
 * Inbound text-body extraction for Weixin.
 *
 * Three concerns are folded in here so the poll loop only has to call
 * one function:
 *
 * 1. **Plain TEXT items.** Concatenated in order with no separator —
 *    iLink frequently splits a long single message into adjacent
 *    TEXT items, and the user expected one continuous string.
 * 2. **Quoted message (`ref_msg`).** When a TEXT item carries a
 *    `ref_msg.message_item`, surface the quote as a `[引用: …]\n`
 *    prefix so the agent sees the conversational context. Quoted
 *    media is *not* included in the text — the caller's media
 *    pipeline picks it up separately.
 * 3. **Voice-to-text fallback.** When a VOICE item carries
 *    `voice_item.text` (server-side STT), use the text directly
 *    instead of treating the message as media-only.
 *
 * Media items themselves are *not* included in the returned text —
 * they ride the `attachments` channel via [`media-download.ts`].
 */
import { MessageItemType, type MessageItem, type WeixinMessage } from "../api/types.js";

/**
 * Extract a plain-text body for the agent. Returns `""` when there
 * is no text content; the poll loop uses that as a "media-only"
 * signal so the inbound event still fires when an attachment was
 * decoded but no caption was attached.
 */
export function extractPlainText(msg: WeixinMessage): string {
  const items = msg.item_list ?? [];
  if (items.length === 0) return "";

  // First pass: collect plain TEXT bodies (concatenated, no separator)
  // and remember the first ref_msg we encounter so the leading quote
  // can prefix the result. This keeps the legacy "TEXT + TEXT = one
  // string" behaviour while letting a single quoted message decorate
  // the body.
  let textParts: string[] = [];
  let quote: string | null = null;
  let voiceText: string | null = null;
  for (const item of items) {
    if (item.type === MessageItemType.TEXT) {
      const text = item.text_item?.text;
      if (text && text.length > 0) textParts.push(text);
      if (!quote) quote = renderQuote(item);
    } else if (item.type === MessageItemType.VOICE && item.voice_item?.text) {
      // Server-side STT: surface the recognized text. Multiple voice
      // items in one message are extremely rare in practice; if they
      // happen, the first wins.
      if (!voiceText) voiceText = item.voice_item.text;
    }
  }

  const body = textParts.join("");
  if (body) {
    return quote ? `${quote}\n${body}` : body;
  }
  // No text → fall back to voice STT result if any.
  if (voiceText) {
    return quote ? `${quote}\n${voiceText}` : voiceText;
  }
  return "";
}

/**
 * Render a `ref_msg` as a `[引用: …]` line, or `null` when the item
 * has no quote or the quote is purely media. Quoted media is the
 * caller's job: it gets surfaced through the media-download pipeline,
 * not the text body.
 */
function renderQuote(item: MessageItem): string | null {
  const ref = item.ref_msg;
  if (!ref) return null;
  const refItem = ref.message_item;
  if (refItem && isMediaTypeRef(refItem)) return null;
  const parts: string[] = [];
  if (ref.title) parts.push(ref.title);
  if (refItem?.type === MessageItemType.TEXT) {
    const refText = refItem.text_item?.text;
    if (refText) parts.push(refText);
  }
  if (parts.length === 0) return null;
  return `[引用: ${parts.join(" | ")}]`;
}

function isMediaTypeRef(item: MessageItem): boolean {
  return (
    item.type === MessageItemType.IMAGE
    || item.type === MessageItemType.VIDEO
    || item.type === MessageItemType.FILE
    || item.type === MessageItemType.VOICE
  );
}
