# Tool approvals

*The native tool-approval card and the pending-prompt queue behind it: `app/ios/App/Core/ChatApprovals.swift`, `ChatStore.approvalObserveFrame`, and `app/ios/App/Screens/ApprovalCardView.swift`.*

A tool call whose declared resources aren't already granted blocks on the
gateway's approval gate, which fans out `approval_requested` and **denies itself
after 5 minutes** if nobody answers.

## Why the card is native

The card is **NATIVE**, mounted inside the composer dock above the pill (so it
rides the keyboard and inflates the web bottom inset), and the pending set is
derived **natively in `pushFrame`** — NOT mirrored from the webview:

- Frames buffered offscreen can overflow and be dropped.
- The sync loop restores rows but not pending prompts.

So a web-held queue could lose the only way to answer a gate that is about to
deny.

## Four inputs, one per way a prompt appears or goes away

- **`approval_requested`** — deduped, because the gate's waker re-fires on the
  newest queue entry.
- **`approval_resolved`** — broadcast to EVERY session's sink, so it is matched
  by prompt id, never by session.
- **`subscribe_state.pending_approvals`** — the authoritative set, REPLACES the
  queue.
- **`tool_completed`** — a **timed-out** gate broadcasts NO resolution, so the
  completion is the only signal that retires the card.

Answering dismisses optimistically and echoes `Frame::ResolveApproval` over the
FFI `chat_resolve_approval` (leg-generic; both legs share the outbound pump); a
leg that can't carry it raises a notice, because the decision is then lost and
the gate will deny on its own.

## Two ids, don't confuse them

- **`call_id`** is minted per prompt (one call can prompt more than once via the
  mid-call `ApprovalHandle`) and is what a resolve answers with.
- **`tool_call_id`** is the BLOCKED TOOL CALL — the id `tool_started` /
  `tool_completed` carry — and is what the work block badges.

## Exactly two answers

**The card offers exactly two answers — approve and deny.** The gate also
accepts an `approve_always` (a standing, session-wide grant covering every
resource the call touches) and the web chat / TUI both offer it, but the phone
deliberately does not: a mis-tap is likeliest here, and a standing grant is the
one decision the user can't walk back by paying more attention next time. The
FFI enum (`api::ApprovalDecision`) omits the variant outright, so the app cannot
send it even by accident — but a verdict given on ANOTHER client still arrives
and renders (see the label under [What the transcript shows](#what-the-transcript-shows)).
Don't "restore" the third button without re-deciding this.

## What the transcript shows

The transcript shows the process, never the prompt: the blocked step reads
"waiting for approval" (glyph BREATHES rather than pulses — nothing is
executing), and after the decision it carries a permanent
approved / always-approved / denied label.

That label is **durable**: `ToolResultMeta::approval` persists it on the tool
result, so a reload re-labels the same step (`ChatWorkStep.approval` on REST
rows, `WireWorkStep.approval` in the `subscribe_state` snapshot). A `deny` also
still reads red via the existing `denied` tool status.

## Board approvals are a different plane

A project card's run can park on the same gate, and answering it is **not**
this code. The two differ in every layer:

| | Chat | Board |
|---|---|---|
| transport | live frames on the device channel | REST (`POST …/issues/{n}/approval`) |
| where the pending set comes from | `ChatStore.approvalObserveFrame` on the frame stream | replaying the card's `events` by `call_id` |
| answers offered | approve / **approve-always** / deny | approve / deny only |
| where it is shown | the composer dock's `ApprovalCardView` | the board's Waiting strip, and the card's own dock |

**Approve-always is deliberately absent from the board.** It widens a policy,
and neither the Waiting strip's compact row nor a card's dock has room to show
*what* was widened — the same rule this file already states for why the chat
card offers three and not four.

The types are separate on purpose: `PendingApproval` is the chat's (it carries
a `toolCallId`, a params preview and the resource lines the frame supplies),
`IssueApprovalPrompt` is the board's (a `call_id`, a tool, a summary and who
asked — everything a timeline entry has). They are genuinely different planes,
not two spellings of one, and `ApprovalCardView` is shared by taking the chat's
shape as its input.

The live queue is the truth on both sides. On the board that matters more,
because the pending set is *replayed from history*: a gateway restart drops
every parked prompt without writing a resolution, and a timed-out prompt
self-denies the same way — so an entry can name a prompt nothing is waiting
for, which is why answering one tolerates a 404 and reads it as "already
closed". The card's `approval_pending` flag comes off the live queue rather
than the timeline for exactly that reason.

## Related

- The chat-list row's parked-gate glyph is the
  [Chat-list approval mark](chat-list.md).
- The board's side is [projects.md](projects.md) §3.1 (the Waiting strip) and
  §3.3 (the card's dock).
- The `-baybo-demo-approval` harness flag is documented in [testing.md](testing.md).
