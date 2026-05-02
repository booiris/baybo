/** Per-tool OAuth scope manifest.
 *
 * Lark scopes are per-API and have to be requested at OAuth-grant
 * time (RFC 6749 §3.3) — Aura's UAT pipeline needs to know which
 * scopes each MCP tool calls so:
 *   1. The device-flow request asks for them up front, otherwise
 *      the user authorizes "offline_access" only and every privileged
 *      tool returns 99991663 (no scope) → reauth-loop.
 *   2. A cached UAT issued for a narrower scope set gets dropped +
 *      reauth'd if a tool now needs a wider one (otherwise the same
 *      99991663 loop just under different cover).
 *
 * Scope names mirror openclaw's `tool-scopes.ts` mapping (ported by
 * 1:1 lookup), since openclaw runs against the same Feishu OAuth
 * surface and has the operationally-tested set.
 *
 * Reads + writes share one space: a tool that does both
 * (calendar_event_create) lists both create AND update, since Feishu
 * splits some mutations across micro-scopes. The auth flow merges +
 * dedupes when sending the device authorization request. */

export const TOOL_SCOPES = {
  feishu_who_am_i: ["contact:user.base:readonly"],
  feishu_get_user: [
    "contact:contact.base:readonly",
    "contact:user.base:readonly",
  ],
  feishu_search_user: ["contact:user:search"],

  feishu_calendar: ["calendar:calendar:read"],
  feishu_calendar_event: ["calendar:calendar.event:read"],
  feishu_calendar_event_create: [
    "calendar:calendar.event:create",
    "calendar:calendar.event:update",
  ],
  feishu_calendar_event_update: ["calendar:calendar.event:update"],
  feishu_calendar_event_delete: ["calendar:calendar.event:delete"],
  feishu_freebusy: ["calendar:calendar.free_busy:read"],
  feishu_freebusy_batch: ["calendar:calendar.free_busy:read"],

  feishu_wiki: ["wiki:space:read", "wiki:node:retrieve"],
  feishu_search_doc: ["search:docs:read"],
  feishu_doc_comments: ["wiki:node:read", "docs:document.comment:read"],

  feishu_bitable_records: ["base:record:retrieve"],
  feishu_bitable_record_create: ["base:record:create"],
  feishu_bitable_record_update: ["base:record:update"],
  feishu_bitable_record_delete: ["base:record:delete"],

  feishu_sheet_read_range: [
    "sheets:spreadsheet.meta:read",
    "sheets:spreadsheet:read",
  ],

  // The MCP doc proxy tools route through Feishu's hosted MCP gateway
  // (mcp.feishu.cn) which negotiates its own scope set. Listed here
  // for completeness so the auth flow asks for the underlying docx
  // scopes the gateway requires; if Lark expands the gateway's scope
  // semantics this list will need to track.
  feishu_fetch_doc: ["docx:document:readonly", "wiki:node:read"],
  feishu_create_doc: [
    "docx:document:create",
    "docx:document:write_only",
    "wiki:node:create",
    "wiki:node:read",
  ],
  feishu_update_doc: [
    "docx:document:readonly",
    "docx:document:write_only",
  ],
} as const satisfies Record<string, readonly string[]>;

export type FeishuToolName = keyof typeof TOOL_SCOPES;

/** Return the deduped scope list a UAT must cover before the named
 * tool can run. Unknown tool name returns `[]` — the tool will
 * succeed only if it doesn't actually need any scope (rare; mostly
 * a graceful-degradation hatch for new tools added without updating
 * this manifest). */
export function scopesFor(toolName: string): readonly string[] {
  return (TOOL_SCOPES as Record<string, readonly string[]>)[toolName] ?? [];
}

/** Combine a set of scope arrays into one space-delimited string
 * suitable for the OAuth `scope` parameter, deduped + sorted for
 * stable comparison. */
export function joinScopes(scopes: readonly string[]): string {
  if (scopes.length === 0) return "";
  return [...new Set(scopes)].sort().join(" ");
}

/** Check whether `granted` (a space-delimited or whitespace-delimited
 * scope string from a stored UAT) covers every scope in `required`.
 * `offline_access` is implicit — tools never list it but every UAT
 * has it, so it's not part of the comparison set. */
export function grantedCovers(
  granted: string,
  required: readonly string[],
): boolean {
  if (required.length === 0) return true;
  const have = new Set(
    granted
      .split(/\s+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0),
  );
  for (const need of required) {
    if (!have.has(need)) return false;
  }
  return true;
}
