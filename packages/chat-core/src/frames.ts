// Live frame routing: fold one inbound wire `Frame` into the per-session
// `SessionView` map. Framework-agnostic — the caller injects a `setViews`
// updater (any `useState`-style setter is structurally compatible) and an
// `onUserPreview` hook for the sidebar's last-message preview, so this stays
// free of React and of each app's sidebar/session-summary shape.
import type { Frame } from './chatWs';
import { EMPTY_VIEW, type SessionView } from './model';
import {
  appendReasoningStep,
  appendStreamingDelta,
  applyToolCompletedStep,
  applyTurnState,
  closeActiveWork,
  closeWorkForFinalReply,
  finalizeMessage,
  isStopCancellationNotice,
  markLastWorkCancelled,
  mergeView,
  noticeLevel,
  parseEpochMs,
  pushStatusStep,
  pushToolStartedStep,
} from './transcript';

/** A `useState`-style setter: accepts the next value or an updater
 *  function. React's `Dispatch<SetStateAction<T>>` is assignable to this,
 *  so the caller passes its `setViews` directly without a React dep here. */
export type SetState<T> = (value: T | ((prev: T) => T)) => void;

/** Update the right per-session bucket based on a frame's session_id.
 *  Always operates on the views map via setViews so background
 *  sessions accumulate frames even when not currently viewed. Unread
 *  accounting lives elsewhere — `Frame::SessionActivity` is the single
 *  source of truth for sidebar badges, fired by the gateway's
 *  dispatch observer regardless of subscription state.
 *
 *  `onUserPreview(sessionId, preview)` is called for each user message so
 *  the caller can refresh its sidebar's last-message preview; the
 *  session-list shape stays the caller's concern. */
export function routeInboundFrame(
  frame: Frame,
  setViews: SetState<Record<string, SessionView>>,
  onUserPreview: (sessionId: string, preview: string) => void,
  lastConnectedAt: number,
): void {
  switch (frame.kind) {
    case 'answer_delta': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: appendStreamingDelta(view.transcript, frame.text),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'reasoning': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: appendReasoningStep(view.transcript, frame.text, view.turn?.active ?? null),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'tool_started': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: pushToolStartedStep(
              view.transcript,
              frame.call_id,
              frame.tool,
              frame.label ?? null,
              view.turn?.active ?? null,
            ),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'tool_completed': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: applyToolCompletedStep(
              view.transcript,
              frame.call_id,
              frame.status,
              frame.summary,
              view.turn?.active ?? null,
            ),
          },
        };
      });
      return;
    }
    case 'status': {
      const sid = frame.session_id;
      const text =
        frame.phase === 'compacting'
          ? 'Compacting context…'
          : frame.phase === 'compacted'
            ? 'Context compacted'
            : frame.phase;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: pushStatusStep(view.transcript, text, view.turn?.active ?? null),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'task_list': {
      // Idempotent snapshot — REPLACE the session's checklist wholesale
      // (not a delta). An empty array means the plan is currently empty
      // and the checklist panel hides.
      const sid = frame.session_id;
      setViews((prev) => mergeView(prev, sid, { tasks: frame.tasks }));
      return;
    }
    case 'turn_state': {
      // Server-authoritative turn lifecycle: broadcast at every turn
      // start/end, snapshotted on every Subscribe. Recorded on the view
      // (drives the Cancelled indicator) and reconciled into the
      // transcript's trailing work block (open/elapsed-timer/close).
      // On `active` it also takes over from the optimistic
      // awaiting-reply indicator — the (possibly still empty) work
      // block is the working affordance from here.
      const sid = frame.session_id;
      const startedAt = parseEpochMs(frame.started_at);
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            turn: { active: frame.active, startedAt },
            transcript: applyTurnState(view.transcript, frame.active, startedAt),
            awaitingReply: frame.active ? false : view.awaitingReply,
          },
        };
      });
      return;
    }
    case 'message': {
      const sid = frame.session_id;
      const role: 'user' | 'assistant' = frame.role === 'user' ? 'user' : 'assistant';
      // Any assistant message — replay or live, with or without prior
      // streaming deltas — ends the "awaiting reply" window AND closes the
      // turn's work block (collapsing it to its `Worked Xs` summary). Both
      // are done here as their own setViews rather than threading them
      // through every replay-merge branch below. The close must fire on
      // the replay path too: the gateway stamps the persisted `ordinal`
      // onto the LIVE final reply (see `OutgoingMessage::ordinal`), so it
      // routes through the ordinal branch, not just the live fall-through.
      // Turns are sequential per session, so the open block is always the
      // one this reply ends.
      if (role === 'assistant') {
        setViews((prev) => {
          const view = prev[sid];
          if (!view) return prev;
          const transcript = closeWorkForFinalReply(view.transcript);
          if (transcript === view.transcript && !view.awaitingReply) return prev;
          return { ...prev, [sid]: { ...view, transcript, awaitingReply: false } };
        });
      }
      // Catch-up replay (ordinal set): key by ordinal so React
      // reconciles against rows the REST history fetch already laid
      // down with the same shape, and a duplicate replay is a no-op.
      // Reconciles against locally-leftover rows from a WS drop in
      // the window between local emit and the live frame:
      // * a `streaming` assistant row (drop mid-Delta-stream) is
      //   swallowed by the replay's finalized Message;
      // * a `pending` user row (drop between handleSend and the
      //   live UserEcho) is matched by text;
      // * a *finalized* `msg-*` row (drop after the live frame was
      //   already rendered — this is the common case when only the
      //   assistant `Frame::Message` carries an ordinal and reconnect
      //   replays the user echo that landed during the previous
      //   session) is also matched by role+text within the recent
      //   tail. The gateway zeros `platform_msg_id` on replay (see
      //   `crates/gateway/src/channel/route.rs`), so text is the best
      //   discriminator we have client-side — sending the same text
      //   twice within the drop window would mis-match, but the
      //   failure mode (one duplicate row) is no worse than the
      //   pre-fix baseline. Without these paths the leftover row
      //   would sit alongside the replay forever.
      if (frame.ordinal !== undefined) {
        const replayKey = `hist-${sid}-${frame.ordinal}`;
        // Replays arrive ascending so iteratively overwriting the
        // sidebar preview converges on the freshest user turn — the
        // disconnect window may have hidden a sibling-tab send that
        // the bootstrap snapshot also missed.
        if (role === 'user') {
          const preview = frame.content.trim().length > 0
            ? frame.content
            : ((frame.attachments?.length ?? 0) > 0 ? '[attachment]' : '');
          if (preview) {
            onUserPreview(sid, preview);
          }
        }
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          if (view.transcript.some((r) => r.key === replayKey)) return prev;
          if (role === 'assistant') {
            const lastIdx = view.transcript.length - 1;
            const last = view.transcript[lastIdx];
            if (last?.streaming && last.role === 'assistant') {
              const next = view.transcript.slice();
              // Preserve the streaming row's `createdAt` (stamped at
              // first Delta) — the persisted Message replay is the
              // same logical bubble, the user just saw it earlier.
              next[lastIdx] = {
                ...last,
                key: replayKey,
                role: 'assistant',
                text: frame.content,
                streaming: false,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          if (role === 'user') {
            const matchIdx = view.transcript.findIndex(
              (r) => r.pending && r.role === 'user' && r.text === frame.content,
            );
            if (matchIdx >= 0) {
              const next = view.transcript.slice();
              next[matchIdx] = {
                ...view.transcript[matchIdx],
                key: replayKey,
                role: 'user',
                text: frame.content,
                pending: false,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          // Finalized live row from a prior connection: scan the
          // tail for a non-keyed (`msg-*` / no `hist-` prefix), non-
          // streaming, non-pending row of the same role+text. Window
          // capped at the last 16 rows so we don't replay-walk a
          // 10k-message scrollback.
          //
          // Iterate oldest→newest within the window. Replays arrive
          // in ascending ordinal order, so the first un-claimed
          // matching row is the one this replay belongs to. The
          // newest-first walk we used before inverted the pairing
          // when the same text appeared twice: replay N would claim
          // the *later* row, then replay N+1 would claim the earlier
          // one, leaving the earlier text rendered with the newer
          // ordinal. Rows already re-keyed by a prior replay carry
          // the `hist-` prefix and are skipped, so iterating forward
          // can't re-claim them.
          //
          // `hasAttachments` is also part of the discriminator so an
          // attachment-only row doesn't get re-keyed onto a text-only
          // replay (and vice-versa) when their text happens to be
          // empty for the attachment side.
          const TAIL_WINDOW = 16;
          const start = Math.max(0, view.transcript.length - TAIL_WINDOW);
          const replayHasAttachments = (frame.attachments?.length ?? 0) > 0;
          for (let i = start; i < view.transcript.length; i++) {
            const row = view.transcript[i];
            if (row.streaming || row.pending) continue;
            if (row.key.startsWith('hist-')) continue;
            if (row.role !== role) continue;
            if (row.text !== frame.content) continue;
            if (Boolean(row.hasAttachments) !== replayHasAttachments) continue;
            const next = view.transcript.slice();
            next[i] = { ...row, key: replayKey, text: frame.content };
            return { ...prev, [sid]: { ...view, transcript: next } };
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: [
                ...view.transcript,
                {
                  key: replayKey,
                  role,
                  text: frame.content,
                  createdAt: new Date().toISOString(),
                },
              ],
            },
          };
        });
        return;
      }
      // Live user echo. If this tab sent the message and is still
      // showing the optimistic placeholder, clear `pending` in place
      // (server text wins — sanitization may have rewritten it) and
      // keep the row's React key so the bubble doesn't unmount/
      // remount. Echoes without a matching placeholder (other tab,
      // pre-optimistic bundle, race after Reset wipe) fall through
      // to the normal append path. Decision is made inside the
      // updater because state setters are batched — checking outside
      // can't observe whether the updater found a match.
      const hasAttachments = (frame.attachments?.length ?? 0) > 0;
      // Sidebar preview tracks the freshest user-authored text, so
      // every live user echo (whether this tab sent it or a sibling
      // did) feeds the sidebar — including the attachment-only case,
      // where the placeholder string mirrors the bubble's "[attachment]"
      // fallback so the row doesn't go blank on a media-only send.
      if (role === 'user') {
        const preview = frame.content.trim().length > 0
          ? frame.content
          : (hasAttachments ? '[attachment]' : '');
        if (preview) {
          onUserPreview(sid, preview);
        }
      }
      if (role === 'user' && frame.platform_msg_id) {
        const clientMsgId = frame.platform_msg_id;
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          const idx = view.transcript.findIndex(
            (r) => r.pending && r.clientMsgId === clientMsgId,
          );
          if (idx >= 0) {
            const next = view.transcript.slice();
            next[idx] = {
              ...view.transcript[idx],
              text: frame.content,
              pending: false,
              hasAttachments: hasAttachments || next[idx].hasAttachments,
            };
            return { ...prev, [sid]: { ...view, transcript: next } };
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: finalizeMessage(view.transcript, role, frame.content, hasAttachments, frame.attachments),
            },
          };
        });
        return;
      }
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: finalizeMessage(view.transcript, role, frame.content, hasAttachments),
          },
        };
      });
      return;
    }
    case 'attachment': {
      // Media a tool produced mid-turn (a sent file, a screenshot).
      // Render it as its OWN standalone bubble — deliberately NOT folded
      // into the open work block and NOT closing the turn, so the
      // in-flight reply keeps streaming afterwards.
      if (frame.attachments.length === 0) return;
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: [
              ...view.transcript,
              {
                key: `attachment-${view.transcript.length}-${Date.now()}`,
                role: 'assistant',
                text: '',
                hasAttachments: true,
                attachments: frame.attachments,
                createdAt: new Date().toISOString(),
              },
            ],
          },
        };
      });
      return;
    }
    case 'notice': {
      const sid = frame.session_id;
      // A transient notice is the progress observer's mid-turn narration,
      // NOT the turn's reply: fold it into the open work block as a status
      // step and leave the turn running, exactly like the `status`
      // (compaction) path. Treating it as terminal here is what split one
      // long turn into two `Worked Xs` blocks — the observer collapsed the
      // block, then later tool activity opened a fresh one.
      if (frame.transient) {
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: pushStatusStep(view.transcript, frame.text, view.turn?.active ?? null),
              awaitingReply: false,
            },
          };
        });
        return;
      }
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        // A notice is terminal for the turn (slash-command reply,
        // refusal, compaction confirmation, …) — close any open work
        // block so it collapses above the notice instead of dangling.
        // When the notice is a `/stop` that actually cancelled the reply,
        // label that block "Cancelled" — this is the path EVERY tab takes
        // (the notice is broadcast), so an observer agrees with the
        // originator (which marked it optimistically) and with a reload.
        // `markLast` also covers the case where `turn_state{inactive}`
        // already closed the block to "Worked" a moment earlier.
        const closed = closeActiveWork(view.transcript);
        const base = isStopCancellationNotice(frame.text)
          ? markLastWorkCancelled(closed)
          : closed;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: [
              ...base,
              {
                key: `notice-${sid}-${base.length}-${Date.now()}`,
                role: 'system',
                text: '',
                notice: { level: noticeLevel(frame.level), text: frame.text },
                createdAt: new Date().toISOString(),
              },
            ],
            // Some turns reply with `AgentOutput::Notice` and never
            // emit a Delta/Message — slash commands like `/compact`,
            // refusal / error paths, etc. Without this, the working
            // indicator would hang forever for those sends. The notice
            // itself is now the reply, so awaitingReply ends here.
            awaitingReply: false,
            // The terminal notice ends the turn locally — so a frame that
            // lands after it (a tool finishing post-`/stop`, a paced flush)
            // folds into the now-closed block via `ensureWork(active:false)`
            // instead of opening a fresh ticking block below the notice. The
            // authoritative `turn_state{active:false}` confirms this moments
            // later; setting it here just closes the race window.
            turn: { active: false, startedAt: null },
          },
        };
      });
      return;
    }
    case 'approval_requested': {
      const sid = frame.session_id;
      const receivedAt = Date.now();
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            pendingApproval: {
              callId: frame.call_id,
              sessionId: sid,
              tool: frame.tool,
              description: frame.description ?? null,
              paramsPreview: frame.params_preview,
              accesses: frame.accesses,
              receivedAt,
            },
            // Agent has stopped to ask the user something — it's no
            // longer composing. The approval card is the activity
            // signal now; suppress the typing dots so the two don't
            // stack and contradict each other.
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'pending_approvals_snapshot': {
      // Server's authoritative list of pending approval call_ids for
      // this session, sent once per Subscribe. Reconcile against our
      // local card: drop if it (a) pre-dates the most recent
      // reconnect — i.e. could plausibly be stale from before — AND
      // (b) is missing from the snapshot. Cards stamped after the
      // last reconnect came in through live broadcast and are
      // protected from the race where a fresh approval arrives in
      // the microsecond gap between the server's subscribe
      // registration and snapshot send.
      const sid = frame.session_id;
      const callIds = new Set(frame.call_ids);
      setViews((prev) => {
        const view = prev[sid];
        if (!view?.pendingApproval) return prev;
        const pa = view.pendingApproval;
        if (pa.receivedAt >= lastConnectedAt) return prev;
        if (callIds.has(pa.callId)) return prev;
        return { ...prev, [sid]: { ...view, pendingApproval: null } };
      });
      return;
    }
    case 'approval_resolved': {
      setViews((prev) => {
        // Walk every session bucket since we don't know which one
        // the call_id belongs to. Map is small (~tabs visited), so
        // this is cheap. Return `prev` unchanged when no card matches
        // — the call_id may belong to an already-resolved session, and
        // a fresh object would force every SessionRow to re-render.
        let next: Record<string, SessionView> | null = null;
        for (const [sid, view] of Object.entries(prev)) {
          if (view.pendingApproval?.callId === frame.call_id) {
            next ??= { ...prev };
            next[sid] = { ...view, pendingApproval: null };
          }
        }
        return next ?? prev;
      });
      return;
    }
    default:
      // history_snapshot / start_bot / stop_bot / slash_manifest /
      // subscribe / unsubscribe / register / register_ack / reset are
      // not expected on the web client (the SDK strips most of them
      // before they reach onFrame; the rest are debug noise).
      return;
  }
}
