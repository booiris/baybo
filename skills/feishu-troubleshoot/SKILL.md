---
name: feishu-troubleshoot
description: |
  How to interpret and respond to common Feishu MCP tool errors —
  authorization denied / expired, scope mismatch, approval timeout,
  group-chat impersonation rejection, "bot not in chat", Lark API
  rate limits.
when_to_use: |
  A Feishu MCP tool just returned `isError: true`, or a previous
  call's result text starts with one of the recognised error
  patterns below.
---

# Feishu Troubleshooting

## Authorization errors

**`auth_failed: denied` ("the user declined the authorization request")**
The user clicked Deny on the OAuth card. Don't retry the same operation — ask them what they want differently first. They may not realise the operation is read-only / safe; if it's a low-risk read, explain why you need the access and let them re-trigger.

**`auth_failed: expired` ("the authorization page timed out")**
The user didn't click Approve within ~4 minutes. Just ask the same question again — that re-triggers a fresh auth card with a new code.

**`subject verification failed` / `does not match the requesting user`**
In a group chat someone other than the conversing user clicked the auth link. Restart the flow and tell the user to click the link THEMSELVES, not delegate it to a coworker. Ideal: do auth in a 1:1 DM with the bot rather than in a group.

**`UAT scope insufficient for tool`** (logged, not LLM-visible)
A previously-cached UAT didn't carry the scope this tool needs — Aura's auto-auth will trigger a fresh OAuth flow with the wider scope. Just retry from the agent's side; the user sees one new auth card.

## API errors

**`Feishu API error 230002: bot is not in the chat`**
The bot needs to be a member of the chat to read its messages or members. Tell the user to add the bot to the chat first.

**`Feishu API error 99991663` / `99991664`**
Stale UAT. Aura's auto-retry should drop + re-auth automatically; if you see this in a tool reply, the auto-retry already failed once after a fresh grant — likely a permission / scope issue masquerading as a token error. Check what scope the tool needs vs what was granted (see `feishu-channel-rules` for the scope manifest).

**`Feishu API error 230020` (rate limit)**
Lark caps OAPI calls per app + per minute. Back off — wait a few seconds and retry once. If it persists, suggest the user wait or switch to a less-frequent query pattern.

## Approval errors (write tools only)

**`denied the write` ("user denied the write — do NOT retry")**
The user clicked Deny on the write-approval card. They explicitly refused this operation. Don't try variants of the same write; ask them what they actually want.

**`approval card timed out after 300s with no response`**
User missed the card. Ask the user again with a clearer summary; that re-fires a fresh approval.

**`requires an in-chat approval gate but the platform didn't configure one`**
Fail-closed defense: Aura's MCP server didn't wire the approval handler. This is a configuration error on the operator side — surface to the user that the bot can't write right now and ask them to reach out to whoever runs Aura.

## Network errors

**`refresh network error: ECONNRESET` (logged)**
Transient. Aura keeps the UAT around and the next sweep retries. No agent action needed unless the same error repeats over 10+ minutes.
