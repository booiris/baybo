import { MessageItemType, type WeixinMessage } from "../api/types.js";

/**
 * Concatenate all TEXT items from a WeixinMessage into a single string.
 * Media (IMAGE/VIDEO/FILE/VOICE) items and quoted `ref_msg` references
 * are ignored — the current BotChannel wire contract is text-only, so
 * media must be dropped before we reach the gateway.
 *
 * Returns `""` when there is no text content; the poll loop uses that
 * as a signal to skip emitting the inbound entirely (a bare image
 * without a caption is indistinguishable from a no-op from the agent's
 * perspective).
 */
export function extractPlainText(msg: WeixinMessage): string {
  if (!msg.item_list || msg.item_list.length === 0) return "";
  const parts: string[] = [];
  for (const item of msg.item_list) {
    if (item.type !== MessageItemType.TEXT) continue;
    const text = item.text_item?.text;
    if (text && text.length > 0) parts.push(text);
  }
  return parts.join("");
}
