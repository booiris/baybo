import telegramifyMarkdown from "telegramify-markdown";

export const TELEGRAM_PARSE_MODE = "MarkdownV2" as const;

export function markdownToTelegram(src: string): string {
  if (!src) return "";
  // remark always serializes with a trailing `\n`; strip it so reply
  // bubbles don't gain a blank line.
  return telegramifyMarkdown(src, "escape").replace(/\n+$/, "");
}
