---
name: feishu-calendar
description: |
  How to read and manage the user's Feishu calendar through the
  feishu_calendar / feishu_calendar_event / feishu_freebusy tool
  family. Read tools work directly; create / update / delete each
  prompt the user with an in-chat approval card.
when_to_use: |
  The user asks about their Feishu calendar — "what's on my calendar
  today", "schedule X for tomorrow", "move the Q3 review", "find a
  free slot for me and Bob next week".
---

# Feishu Calendar

## Tool surface

| Need | Tool | Notes |
|---|---|---|
| Find calendar id | `feishu_calendar action=primary` | Returns the user's main calendar. Use first if you don't have an id. |
| List user's calendars | `feishu_calendar action=list` | All calendars they can see (own + shared). |
| Get calendar metadata | `feishu_calendar action=get` | Needs `calendar_id`. |
| List events in a window | `feishu_calendar_event action=list` | Filter via `start_time` + `end_time` (Unix-seconds-or-millis strings). |
| Get one event | `feishu_calendar_event action=get` | Needs `calendar_id` + `event_id`. Pass `need_attendee=true` for richer payload. |
| **Create event** | `feishu_calendar_event_create` | **Triggers approval card.** No attendees support yet — see "Limitations". |
| **Update event** | `feishu_calendar_event_update` | **Triggers approval card.** Patch semantics — only listed fields change. |
| **Delete event** | `feishu_calendar_event_delete` | **Triggers approval card.** Pass `summary` so the card shows what's being deleted. |
| Free/busy one user | `feishu_freebusy` | Optional `user_id` (omit for self), or `room_id` (mutually exclusive). |
| Free/busy several users | `feishu_freebusy_batch` | `user_ids` array, max 50. |

## Patterns

**"What's on my calendar today?"** → `feishu_calendar action=primary` to get id, then `feishu_calendar_event action=list` with start/end timestamps for today.

**"Schedule X for 2pm tomorrow"** → `feishu_calendar action=primary`, then `feishu_calendar_event_create` with the calendar id + summary + Unix-millis start/end + timezone. The user sees an approval card; only on Approve does the event land.

**"Find a free slot for me and Bob next Tuesday"** → `feishu_freebusy_batch` with `time_min`/`time_max` covering Tuesday + `user_ids: ["ou_self", "ou_bob"]`. Look for gaps in the returned `freebusy_lists`.

**"Move the Q3 review by an hour"** → `feishu_calendar_event action=list` with a wide enough window to find it, then `feishu_calendar_event_update` with the new `start_timestamp` + `end_timestamp` (always change BOTH — Feishu doesn't auto-shift the duration).

**"Cancel the meeting with Carol"** → look it up first (`feishu_calendar_event action=list` or by id), pass the summary into `feishu_calendar_event_delete` so the user sees what's being deleted on the approval card.

## Limitations

- **No attendees on create**: `feishu_calendar_event_create` creates a personal event only. Inviting people needs the `calendar.calendarEventAttendee.create` API which isn't plumbed yet — ask the user to add attendees themselves in Lark UI after the event lands.
- **Recurring events**: `delete` removes the entire recurrence series, not a single occurrence. For one-occurrence cancellations, route the user to Lark UI.
- **All times are Unix-seconds-or-millis strings** in the `timestamp` field, not ISO 8601. The SDK accepts both; default to whatever the user says.

## Approval flow

`_create` / `_update` / `_delete` all send a card to the chat with [Approve] [Deny]. The user has 5 minutes to click; otherwise the tool returns a timeout error. **Don't retry on Deny** — ask the user what they want differently before trying anything else.
