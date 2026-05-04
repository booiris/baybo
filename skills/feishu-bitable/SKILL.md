---
name: feishu-bitable
description: |
  How to read and mutate Feishu Bitable records through the
  feishu_bitable_records / feishu_bitable_record_* tool family.
  Read tool works directly; create / update / delete each prompt
  the user with an in-chat approval card carrying the field map.
when_to_use: |
  The user asks to query or change data in a Feishu Bitable —
  "look up the open bugs", "log this meeting", "add a row for X",
  "update the status of Y to Done".
---

# Feishu Bitable

## Tool surface

| Need | Tool | Notes |
|---|---|---|
| List/filter records | `feishu_bitable_records` | Read-only. Forwards Feishu's `filter` / `sort` / `view_id` / `field_names` expressions verbatim. |
| **Create one record** | `feishu_bitable_record_create` | **Triggers approval.** Single-record only — see "Batch" below. |
| **Update one record** | `feishu_bitable_record_update` | **Triggers approval.** Partial — only listed fields change. |
| **Delete one record** | `feishu_bitable_record_delete` | **Triggers approval.** Pass `preview` so the user sees what's being deleted. |

## Patterns

**"How many open bugs?"** → `feishu_bitable_records` with `filter: 'CurrentValue.[Status]="Open"'` (Feishu's bitable filter syntax). Returns matching records with `total` + paginate via `page_token` if needed.

**"Add a row: Title=Foo, Status=Open"** → `feishu_bitable_record_create` with `fields: { Title: "Foo", Status: "Open" }`. The user sees an approval card with the full field map and clicks Approve.

**"Mark issue X as Done"** → first find the row id via `feishu_bitable_records` with a filter, then `feishu_bitable_record_update` with `record_id` + `fields: { Status: "Done" }`. Other fields keep their values (partial update).

**"Delete the duplicate row"** → `feishu_bitable_record_delete` with `record_id` AND `preview: 'Title="X", Status="Open"'` so the approval card shows what's being deleted, not just an opaque rec_id.

## Field values

Bitable's per-cell type union is huge. Pass values matching Feishu's per-type encoding:

| Cell type | Value shape |
|---|---|
| Text / number / checkbox | `string` / `number` / `boolean` |
| Hyperlink | `{ text: "label", link: "https://..." }` |
| Person/user-link | `[{ id: "ou_..." }]` |
| Multi-select | `["opt1", "opt2"]` |
| Attachment | `[{ file_token: "...", name: "..." }]` |

Use **field names** (the column labels in Bitable UI), NOT internal `fld_...` ids.

## Batch

There's no batch-create / batch-update tool yet. Do NOT loop `_create` for many rows — it would fire one approval card per row, which is a terrible UX. Instead, ask the user to do bulk imports in the Bitable UI.

## Approval flow

Same pattern as calendar writes: `_create` / `_update` / `_delete` send a card with the operation summary + (for create/update) the field map in a fenced code block. User has 5 minutes; **don't retry on Deny**.
