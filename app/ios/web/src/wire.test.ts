/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import transcriptSource from "./Transcript.tsx?raw";
import { restStepToWork, wireStepToWork } from "./Transcript";
import type { WireFrame } from "./types";
import type { Frame } from "../../../../sidecars/sdk/channel-ts/src/generated/Frame";

// The frame router in `Transcript.tsx` is a `switch (frame.kind)` over string
// literals with a silent `default: break`. A wire-side rename therefore does not
// fail anything — the frames simply stop being rendered. This suite pins the
// literals from both ends: the `satisfies` clauses below fail `tsc --noEmit` if
// the hand-written mirror (`types.ts`) or the ts-rs contract (`Frame`) drops a
// kind, and the source scan fails HERE if the switch and the mirror drift apart.

/// Kinds that mirror a real wire frame the Rust side synthesizes AND the router
/// renders — `wireSentinel.ts`'s `MirroredKind` minus the deck frames, which
/// are pinned there for the native deck shell but deliberately fall through the
/// router's `default` here (no deck UI in the transcript).
const WIRE_KINDS = [
  "message",
  "answer_delta",
  "reasoning",
  "tool_started",
  "tool_completed",
  "approval_requested",
  "approval_resolved",
  "turn_state",
  "subscribe_state",
  "gap",
  "notice",
] as const satisfies readonly Frame["kind"][];

/// Kinds native SYNTHESIZES for the webview (never on the wire): the sync /
/// history API replies and their failure edges.
const NATIVE_KINDS = ["sync_page", "history_page", "sync_failed", "history_failed"] as const;

const HANDLED_KINDS = [...WIRE_KINDS, ...NATIVE_KINDS] satisfies readonly WireFrame["kind"][];

/// The `case "…":` labels of the frame router — the only `switch` in the file.
function routedKinds(): string[] {
  return [...transcriptSource.matchAll(/case "(\w+)":/g)].map((m) => m[1]);
}

describe("the frame router's kind literals", () => {
  it("routes exactly the kinds the mirror declares — no orphan case, no kind falling through the default", () => {
    expect([...routedKinds()].sort()).toEqual([...HANDLED_KINDS].sort());
  });

  it("routes each kind exactly once", () => {
    const routed = routedKinds();
    expect(routed).toHaveLength(new Set(routed).size);
  });

  it("routes every wire-origin kind the gateway can send", () => {
    expect(routedKinds()).toEqual(expect.arrayContaining([...WIRE_KINDS]));
  });

  it("routes every native-synthesized kind", () => {
    expect(routedKinds()).toEqual(expect.arrayContaining([...NATIVE_KINDS]));
  });
});

describe("the field names the reducers read off a parsed frame", () => {
  it("sync_page: since_ordinal / next_cursor / rebased drive REPLACE-vs-merge and the cursor", () => {
    const frame = JSON.parse(
      r(`{"kind":"sync_page","rows":[],"since_ordinal":null,"next_cursor":42,
          "rebased":true,"oldest_ordinal":7,"has_more_older":true}`),
    ) as Extract<WireFrame, { kind: "sync_page" }>;

    expect(frame.since_ordinal).toBeNull();
    expect(frame.next_cursor).toBe(42);
    expect(frame.rebased).toBe(true);
    expect(frame.oldest_ordinal).toBe(7);
    expect(frame.has_more_older).toBe(true);
  });

  it("history_page: has_more / oldest_ordinal drive scroll-up paging", () => {
    const frame = JSON.parse(
      r(`{"kind":"history_page","rows":[],"oldest_ordinal":3,"newest_ordinal":9,"has_more":false}`),
    ) as Extract<WireFrame, { kind: "history_page" }>;

    expect(frame.has_more).toBe(false);
    expect(frame.oldest_ordinal).toBe(3);
  });

  it("message: ordinal is the sync cursor, platform_msg_id the echo-dedup key", () => {
    const frame = JSON.parse(
      r(`{"kind":"message","role":"user","content":"hi","ordinal":12,"platform_msg_id":"pm-1"}`),
    ) as Extract<WireFrame, { kind: "message" }>;

    expect(frame.ordinal).toBe(12);
    expect(frame.platform_msg_id).toBe("pm-1");
  });

  it("approval_requested: tool_call_id badges the step, call_id answers the prompt — two ids, not one", () => {
    const frame = JSON.parse(
      r(`{"kind":"approval_requested","call_id":"prompt-1","tool_call_id":"call-9","tool":"Bash"}`),
    ) as Extract<WireFrame, { kind: "approval_requested" }>;

    expect(frame.call_id).toBe("prompt-1");
    expect(frame.tool_call_id).toBe("call-9");
    expect(frame.call_id).not.toBe(frame.tool_call_id);
  });

  it("subscribe_state: pending_approvals carry the tool_call_id the work step is keyed by", () => {
    const frame = JSON.parse(
      r(`{"kind":"subscribe_state","session_id":"s1","turn":{"active":true,"started_at":"2026-07-12T10:00:00Z"},
          "work_steps":[{"kind":"tool","call_id":"call-9","tool":"Bash"}],
          "pending_approvals":[{"call_id":"prompt-1","tool_call_id":"call-9","tool":"Bash","params_preview":"ls"}]}`),
    ) as Extract<WireFrame, { kind: "subscribe_state" }>;

    const step = wireStepToWork((frame.work_steps ?? [])[0]);
    const card = (frame.pending_approvals ?? [])[0];
    expect(step).toMatchObject({ kind: "tool", callId: "call-9" });
    expect(card.tool_call_id).toBe("call-9");
    expect(frame.turn.started_at).toBe("2026-07-12T10:00:00Z");
  });

  it("tool_started / tool_completed pair by call_id — the id a resumed turn's step still carries", () => {
    const started = JSON.parse(
      r(`{"kind":"tool_started","call_id":"call-9","tool":"Bash","label":"Bash(ls)"}`),
    ) as Extract<WireFrame, { kind: "tool_started" }>;
    const completed = JSON.parse(
      r(`{"kind":"tool_completed","call_id":"call-9","status":"ok","summary":"3 files","approval":"approve"}`),
    ) as Extract<WireFrame, { kind: "tool_completed" }>;

    expect(started.call_id).toBe(completed.call_id);
    expect(completed.status).toBe("ok");
    expect(completed.approval).toBe("approve");
  });

  it("a REST work step uses the tool_* names, NOT the wire ones — one Row, two field vocabularies", () => {
    const item = JSON.parse(
      r(`{"kind":"tool","tool":"Bash","tool_label":"Bash(ls)","tool_status":"error","tool_summary":"exit 1"}`),
    ) as NonNullable<Extract<WireFrame, { kind: "sync_page" }>["rows"][number]["steps"]>[number];

    expect(restStepToWork(item)).toMatchObject({ label: "Bash(ls)", status: "error", summary: "exit 1" });
  });
});

/// Collapse a raw multi-line JSON literal back to something `JSON.parse` takes.
function r(json: string): string {
  return json.replace(/\s+/g, " ");
}
