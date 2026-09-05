import type { components } from "../../../../web/src/api/schema";
import type { Actor, IssueAttachment, IssueDetail, IssueEvent, IssueRun } from "./types";

type GeneratedIssue = components["schemas"]["IssueDto"];
type GeneratedEvent = components["schemas"]["IssueEventDto"];
type GeneratedRun = components["schemas"]["IssueRunDto"];
type GeneratedActor = components["schemas"]["ActorDto"];
type GeneratedAttachment = components["schemas"]["IssueAttachmentDto"];

type Undefinedify<T> = T extends object
  ? {
      [K in keyof T]: undefined extends T[K] ? Undefinedify<Exclude<T[K], null>> : Undefinedify<T[K]>;
    }
  : T;

type Assert<T extends true> = T;

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
