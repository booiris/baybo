import type * as lark from "@larksuiteoapi/node-sdk";

/**
 * Strip Feishu's `@_user_N` / `@_all` mention placeholders from an
 * inbound message body. Lark's `NormalizedMessage.content` keeps the
 * placeholders verbatim so the SDK consumer can decide whether to
 * preserve, replace, or strip them; for the agent's text input we want
 * the clean prose (`@bot what is the weather` → `what is the weather`).
 *
 * The bot's own mention is the most important to remove — leaving it in
 * makes the LLM's input read like the bot is talking to itself. Other
 * mentions are stripped too so the agent sees the same body whether the
 * user typed `@alice please ack` or `please ack`; if a downstream skill
 * needs the addressee list it can re-derive it from `mentions`.
 */
export function cleanInboundContent(
  content: string,
  mentions: ReadonlyArray<lark.MentionInfo>,
): string {
  if (mentions.length === 0) return content.trim();
  let out = content;
  for (const m of mentions) {
    if (!m.key) continue;
    out = out.replaceAll(m.key, "");
  }
  return out.replace(/\s+/g, " ").trim();
}
