---
name: feishu-people
description: |
  How to identify and look up Feishu users — the conversing user
  themselves (feishu_who_am_i), a user by id (feishu_get_user), or
  search by name (feishu_search_user). All three act under the
  conversing user's UAT, so visibility respects their org-chart
  scope, not the bot's app-level permissions.
when_to_use: |
  The agent needs the conversing user's name/email to personalise a
  reply, or has a user_id from another tool's response and needs
  the human details, or has a person's name and needs their open_id
  to mention them / send them a card / query their calendar.
---

# Feishu People

## Tool surface

| Need | Tool | Notes |
|---|---|---|
| Who is the user I'm talking to? | `feishu_who_am_i` | No args. Returns name, avatar, email, open_id of the OAuth subject. Bound to the conversation user — LLM can't probe a different user. |
| Look up by user id | `feishu_get_user` | Pass `user_id` (default `open_id` flavour). Returns name + email + mobile + department + status. |
| Search by name keyword | `feishu_search_user` | Free-text against name; returns matches ranked by org-chart proximity. Useful when the agent only has a name. |

## Patterns

**"Reply with my name"** → `feishu_who_am_i` once at the start of a session, cache the result for the rest of the conversation. Don't call repeatedly.

**"Bob says..." → who's Bob?** → `feishu_search_user` with `query: "Bob"`. Returns matches; pick the one whose org context fits.

**"Who owns the X bitable?"** → `feishu_bitable_records` returns `created_by` / `last_modified_by` with an open_id; pass that to `feishu_get_user` for the human details.

## Auth scope nuance

These read tools all run under the conversing user's UAT — Aura asks for the OAuth grant once per session, the user clicks Approve, then all three work. If the user denies the grant, all three return `auth_failed: denied` and the agent should explain it can't look people up without the user's approval.

The first call in a session may pause for ~30s while the user clicks the auth link in chat. Subsequent calls are fast — the UAT is cached and refreshed in the background.
