---
name: feishu-docs
description: |
  How to find, read, create, and update Feishu cloud docs through
  feishu_search_doc / feishu_fetch_doc / feishu_create_doc /
  feishu_update_doc, plus wiki structure browsing via feishu_wiki
  and comment reading via feishu_doc_comments. The fetch/create/
  update trio routes through Feishu's hosted MCP gateway
  (mcp.feishu.cn) for proper Markdown round-tripping.
when_to_use: |
  The user asks to find / read / write a Feishu doc (docx, doc,
  sheet, slides) — "summarise this doc", "create a meeting notes
  doc", "add a section to the project plan", "find docs about Q3".
---

# Feishu Docs

## Tool surface

| Need | Tool | Notes |
|---|---|---|
| Find docs by keyword | `feishu_search_doc` | Free-text across docx + doc + sheet + bitable. Returns tokens + titles. |
| Read doc as Markdown | `feishu_fetch_doc` | Routes via `mcp.feishu.cn` for Markdown conversion. Supports `offset` + `limit` for large docs. |
| Browse wiki structure | `feishu_wiki` | `action: get_space` (info) or `list_nodes` (children of a node, or root). |
| Read comments on a doc | `feishu_doc_comments` | Filter by `is_solved` for "any open comments?" workflows. Needs `file_type`. |
| **Create doc from Markdown** | `feishu_create_doc` | NO approval gate — creating a new doc is low-blast-radius. Validates `markdown` + `title` required. |
| **Update doc with Markdown** | `feishu_update_doc` | Mode-aware: destructive modes (overwrite, replace_all/range, delete_range) trigger approval; additive modes (append, insert_*) skip it. |

## Patterns

**"What does this doc say?"** → `feishu_fetch_doc` with `doc_id`. If the doc is large enough that the agent's context would blow, paginate with `offset` + `limit`.

**"Find docs about Q3 planning"** → `feishu_search_doc` with `query: "Q3 planning"`, then `feishu_fetch_doc` on the most relevant token.

**"What's in our wiki under engineering?"** → `feishu_wiki action=list_nodes` with the space id, optionally drilling into a `parent_node_token` to walk children.

**"Take meeting notes"** → `feishu_create_doc` with the Markdown body + title. Optionally drop into a wiki node via `wiki_node` (mutually exclusive with `folder_token` / `wiki_space`).

**"Add an action items section to the project doc"** → `feishu_update_doc` with `mode: "append"` + the Markdown to add. Append is additive, no approval needed.

**"Replace the timeline section"** → `feishu_update_doc` with `mode: "replace_range"` + `selection_by_title: "## Timeline"` + new Markdown. **Triggers approval** — destructive.

**"Are there any unresolved comments on the API doc?"** → `feishu_doc_comments` with `file_token`, `file_type: "docx"`, `is_solved: false`.

## Update modes

| Mode | What it does | Needs selection? | Approval? |
|---|---|---|---|
| `append` | Adds at end | No | No |
| `insert_before` / `insert_after` | Inserts at a selection | Yes | No |
| `replace_range` | Replaces a selection | Yes | YES |
| `replace_all` | Replaces whole body | No | YES |
| `overwrite` | Same as replace_all | No | YES |
| `delete_range` | Deletes a selection | Yes | YES |

**Selection** = either `selection_with_ellipsis: "start text...end text"` OR `selection_by_title: "## Heading"` (mutually exclusive).

## Tokens

Doc tokens look like `doxcn...` (docx), `doccn...` (legacy doc), `shtcn...` (sheet), `bascn...` (bitable). Wiki tokens: `wikcn...`. Pass these as-is — the gateway parses both raw tokens and full URLs.

## Approval flow

Only destructive `feishu_update_doc` modes trigger approval. The card shows the mode + doc id + selection target + a Markdown preview. **Don't retry on Deny.**
