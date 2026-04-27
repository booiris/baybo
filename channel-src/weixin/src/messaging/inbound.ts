/**
 * Inbound text-body extraction for Weixin. Three concerns are folded
 * in here so the poll loop only has to call one function:
 *
 * 1. **Plain TEXT items.** Concatenated in order — multi-segment
 *    messages (typing → enter sends a single TEXT item, but the
 *    server can split long text) come out as one string.
 * 2. **Quoted message (`ref_msg`).** When a TEXT item carries
 *    `ref_msg.message_item`, surface the quote as a `[引用: …]\n`
 *    prefix so the agent sees the conversational context.
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
  const parts: string[] = [];
  for (const item of items) {
    const text = textBodyForItem(item);
    if (text) parts.push(text);
  }
  return parts.join("\n");
}

function textBodyForItem(item: MessageItem): string {
  if (item.type === MessageItemType.TEXT) {
    const text = item.text_item?.text;
    if (!text) return "";
    const ref = item.ref_msg;
    if (!ref) return text;
    // Quoted media — caller surfaces the actual file via the media
    // download path, so the body just carries the new text.
    const refItem = ref.message_item;
    if (refItem && isMediaTypeRef(refItem)) return text;
    const parts: string[] = [];
    if (ref.title) parts.push(ref.title);
    if (refItem) {
      const refText = textBodyForItem(refItem);
      if (refText) parts.push(refText);
    }
    if (parts.length === 0) return text;
    return `[引用: ${parts.join(" | ")}]\n${text}`;
  }
  if (item.type === MessageItemType.VOICE && item.voice_item?.text) {
    // Server-side STT result. Treat the whole message as text so the
    // agent doesn't have to do anything special with the audio.
    return item.voice_item.text;
  }
  return "";
}

function isMediaTypeRef(item: MessageItem): boolean {
  return (
    item.type === MessageItemType.IMAGE
    || item.type === MessageItemType.VIDEO
    || item.type === MessageItemType.FILE
    || item.type === MessageItemType.VOICE
  );
}
