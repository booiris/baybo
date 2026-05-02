import test from "node:test";
import assert from "node:assert/strict";

import { TOOL_SCOPES, grantedCovers, joinScopes, scopesFor } from "../dist/auth/scopes.js";

test("scopesFor returns the per-tool scope list, [] for unknown tools", () => {
  assert.deepEqual(
    scopesFor("feishu_calendar_event_create"),
    ["calendar:calendar.event:create", "calendar:calendar.event:update"],
  );
  assert.deepEqual(scopesFor("feishu_freebusy"), ["calendar:calendar.free_busy:read"]);
  // Unknown tool returns empty — graceful degradation for tools added
  // without updating the manifest.
  assert.deepEqual(scopesFor("feishu_brand_new_unmapped_tool"), []);
});

test("scopesFor covers every UAT-requiring tool the server registers", () => {
  // Sanity: every entry in TOOL_SCOPES MUST be a string-keyed
  // non-empty array. Catches typos / accidentally-empty entries
  // (which would silently bypass scope plumbing — a regression
  // back to the Codex #2 condition).
  for (const [name, scopes] of Object.entries(TOOL_SCOPES)) {
    assert.ok(scopes.length > 0, `${name}: scope list must be non-empty`);
    for (const s of scopes) {
      assert.equal(typeof s, "string", `${name}: scope ${JSON.stringify(s)} must be string`);
      assert.ok(s.length > 0, `${name}: scope must be non-empty`);
    }
  }
});

test("joinScopes dedupes and sorts so equality comparisons are stable", () => {
  assert.equal(joinScopes([]), "");
  assert.equal(
    joinScopes(["b", "a", "c", "a"]),
    "a b c",
  );
  // Real-world: union of two tools' scopes.
  assert.equal(
    joinScopes([
      ...scopesFor("feishu_calendar_event"),
      ...scopesFor("feishu_calendar_event_create"),
    ]),
    "calendar:calendar.event:create calendar:calendar.event:read calendar:calendar.event:update",
  );
});

test("grantedCovers: true when granted ⊇ required, false otherwise", () => {
  // Empty required: trivially covered.
  assert.equal(grantedCovers("", []), true);
  assert.equal(grantedCovers("a b c", []), true);

  // Full coverage.
  assert.equal(grantedCovers("a b c", ["a"]), true);
  assert.equal(grantedCovers("a b c", ["a", "b"]), true);
  assert.equal(grantedCovers("a b c", ["a", "b", "c"]), true);

  // Missing one.
  assert.equal(grantedCovers("a b", ["a", "b", "c"]), false);
  assert.equal(grantedCovers("a", ["b"]), false);
  assert.equal(grantedCovers("", ["a"]), false);
});

test("grantedCovers tolerates whitespace variation in granted string", () => {
  // Lark's `scope` field uses space-delimited but real responses have
  // varied — handle multi-space and leading/trailing whitespace.
  assert.equal(grantedCovers("  a  b   c ", ["a", "c"]), true);
  assert.equal(grantedCovers("a\tb\nc", ["b"]), true);
});

test("scope manifest covers all the user-facing tool names server.ts registers", () => {
  // Every tool that uses runUatTool / callDocProxyTool MUST have a
  // scope mapping. If the server registers a UAT tool whose name
  // isn't in TOOL_SCOPES, scopesFor returns [] silently and the
  // OAuth grant won't ask for what the API needs — i.e. Codex
  // finding #2 returns. Cross-check by enumerating the names we
  // know the server registers as of slice C.D end.
  const expected = [
    "feishu_who_am_i",
    "feishu_get_user",
    "feishu_search_user",
    "feishu_calendar",
    "feishu_calendar_event",
    "feishu_calendar_event_create",
    "feishu_calendar_event_update",
    "feishu_calendar_event_delete",
    "feishu_freebusy",
    "feishu_freebusy_batch",
    "feishu_wiki",
    "feishu_search_doc",
    "feishu_doc_comments",
    "feishu_bitable_records",
    "feishu_bitable_record_create",
    "feishu_bitable_record_update",
    "feishu_bitable_record_delete",
    "feishu_sheet_read_range",
    "feishu_fetch_doc",
    "feishu_create_doc",
    "feishu_update_doc",
  ];
  for (const name of expected) {
    assert.ok(
      scopesFor(name).length > 0,
      `${name}: missing scope mapping in TOOL_SCOPES`,
    );
  }
});
