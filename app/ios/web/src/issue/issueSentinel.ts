// Compile-time drift sentinel — type-only, produces no runtime output.
//
// The card page renders the gateway's issue DTOs verbatim: the ffi hands each
// response on as an untouched `serde_json::Value`, so the JSON reaching this
// bundle IS `IssueDto` / `IssueEventDto` / `IssueRunDto`. Those are utoipa
// `ToSchema`s, not ts-rs, so `scripts/check-ts-bindings.sh` has nothing to say
// about them; this file pins the hand-written mirrors instead, exactly as
// `restSentinel.ts` does for the transcript rows and `wireSentinel.ts` for the
// live frames.
//
// It reads `app/web`'s generated schema rather than generating a second copy —
// both would come from the one committed `docs/openapi.json` through the same
// generator and be byte-identical, and a second copy is a second thing to
// regenerate, i.e. a new drift surface inside the gate built to close one. The
// import is type-only under `noEmit`, so `tsc` follows the path and no bundler
// or pnpm resolution is involved.
//
// **This file is why the mirrors are right.** Written from the Rust source by
// hand, `IssueDetail.assignee` was an `AgentRef`, `IssueRun` carried an
// `agent` object, and `Actor` was externally tagged — three wrong guesses, all
// of which would have failed silently at runtime as a missing `@handle` and a
// blank agent column. The build caught every one.
import type { components } from "../../../../web/src/api/schema";
import type { Actor, IssueAttachment, IssueDetail, IssueEvent, IssueRun } from "./types";

type GeneratedIssue = components["schemas"]["IssueDto"];
type GeneratedEvent = components["schemas"]["IssueEventDto"];
type GeneratedRun = components["schemas"]["IssueRunDto"];
type GeneratedActor = components["schemas"]["ActorDto"];
type GeneratedAttachment = components["schemas"]["IssueAttachmentDto"];

/// utoipa describes every `Option<T>` as `["T", "null"]`, but each one here
/// also carries `skip_serializing_if = "Option::is_none"` — `None` is OMITTED,
/// never encoded as `null`, and absent is the only absence this bundle can
/// observe. Scoped to OPTIONAL properties for exactly that reason: a nullable
/// field serde does not skip stays required in the schema, and there the `null`
/// is real and must survive into the assertion.
type Undefinedify<T> = T extends object
  ? {
      [K in keyof T]: undefined extends T[K] ? Undefinedify<Exclude<T[K], null>> : Undefinedify<T[K]>;
    }
  : T;

type Assert<T extends true> = T;

/// The mirrors narrow `status`, `priority` and a run's `status` to string
/// unions where the schema says `IssueStatusDto` etc. — the generated enums are
/// string unions themselves, so these are the same type and the check is exact.
/// `trigger` is deliberately left wide: the board adds triggers, the card page
/// only prints one, and a narrowed union here would fail the build over a
/// string this page never reads.
type IssueMatches = Assert<
  Undefinedify<Omit<GeneratedIssue, "sub_issues">> extends Omit<IssueDetail, "sub_issues">
    ? true
    : false
>;
type IssueCovers = Assert<
  Omit<IssueDetail, "sub_issues"> extends Undefinedify<Omit<GeneratedIssue, "sub_issues">>
    ? true
    : false
>;

type EventMatches = Assert<
  Undefinedify<Omit<GeneratedEvent, "body">> extends Omit<IssueEvent, "body"> ? true : false
>;

/// A run's `trigger` is the wide one; everything else is pinned both ways.
type RunMatches = Assert<
  Undefinedify<Omit<GeneratedRun, "trigger" | "status">> extends Omit<IssueRun, "trigger" | "status">
    ? true
    : false
>;
type RunCovers = Assert<
  Omit<IssueRun, "trigger" | "status"> extends Undefinedify<Omit<GeneratedRun, "trigger" | "status">>
    ? true
    : false
>;

/// The one that was wrong by hand. An externally-tagged mirror type-checks
/// against nothing here, which is the point.
type ActorMatches = Assert<GeneratedActor extends Actor ? true : false>;
type ActorCovers = Assert<Actor extends GeneratedActor ? true : false>;

type AttachmentMatches = Assert<
  Undefinedify<GeneratedAttachment> extends IssueAttachment ? true : false
>;
type AttachmentCovers = Assert<
  IssueAttachment extends Undefinedify<GeneratedAttachment> ? true : false
>;

// Referenced so `noUnusedLocals` keeps every assertion alive: an unused type
// alias is elided, and an elided assertion asserts nothing.
export type IssueSentinel = [
  IssueMatches,
  IssueCovers,
  EventMatches,
  RunMatches,
  RunCovers,
  ActorMatches,
  ActorCovers,
  AttachmentMatches,
  AttachmentCovers,
];
