// Regenerate `src/pages/projects/commentHintVectors.json` — the one contract
// the Swift port of the composer hint is held to.
//
//   pnpm --filter baybo-web gen:comment-hint-vectors
//
// WHY A SHARED FIXTURE. What a comment does besides being recorded is decided
// by `crates/project/src/comments.rs::comment_delivery`, and it is NOT exposed
// over REST — a composer has to say what sending will do while the text is
// still being typed, so it cannot ask the server. That forces a client-side
// copy per client: `timelineModel.commentHint` here, `CommentHint.swift` on
// iOS. Nothing makes the three agree except tests, and two of them will
// happily stay green while the third drifts.
//
// The JS implementation is the REFERENCE — its own rules are pinned by the
// hand-written cases in `timelineModel.test.ts` / `mentionModel.test.ts`, and
// these vectors pin the exact bytes both ports must produce for those rules.
// Regenerating is therefore only correct once those suites are green;
// otherwise this bakes a bug into the contract and the Swift side dutifully
// reproduces it.
//
// Expect `CommentHintVectorTests` (app/ios) to go red after a regen until the
// Swift side is brought along. That red is the gate working; app/ios has no
// CI (every `ios-*` job is `if: false`), so run it by hand.

import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { commentHint } from '../src/pages/projects/timelineModel.ts';
import { mentionHint } from '../src/pages/projects/mentionModel.ts';

/** A teammate, reduced to what either hint reads off one. */
const agent = (id, handle) => ({ id, handle });
const TEAM = [agent('a-dev', 'dev-1'), agent('a-lead', 'lead')];

/**
 * `[name, issue, runs, team]`. Keep names stable — the Swift suite reports by
 * name, so a rename reads as a new failure rather than a moved one.
 */
const commentCases = [
  ['nobody assigned', { status: 'todo' }, [], TEAM],
  ['nobody assigned, and a run somehow live', { status: 'in_progress' }, [{ status: 'running' }], TEAM],
  ['cancelled outranks everything', { status: 'in_progress', assignee: 'a-dev', cancelled_at_ms: 5 }, [{ status: 'running' }], TEAM],
  ['backlog records only', { status: 'backlog', assignee: 'a-dev' }, [], TEAM],
  ['done records only', { status: 'done', assignee: 'a-dev' }, [], TEAM],
  ['a block outranks a live run', { status: 'in_progress', assignee: 'a-dev', blocked_reason: 'needs the token format' }, [{ status: 'running' }], TEAM],
  ['a block in todo', { status: 'todo', assignee: 'a-dev', blocked_reason: 'why' }, [], TEAM],
  ['held names the ceiling without naming money', { status: 'in_progress', assignee: 'a-dev' }, [{ status: 'held' }], TEAM],
  ['queued', { status: 'in_progress', assignee: 'a-dev' }, [{ status: 'queued' }], TEAM],
  ['running', { status: 'in_progress', assignee: 'a-dev' }, [{ status: 'running' }], TEAM],
  ['idle in progress starts a run', { status: 'in_progress', assignee: 'a-dev' }, [], TEAM],
  ['idle in todo starts a run where it stands', { status: 'todo', assignee: 'a-dev' }, [], TEAM],
  ['idle in review starts a run', { status: 'review', assignee: 'a-dev' }, [], TEAM],
  ['a settled run is not a live one', { status: 'in_progress', assignee: 'a-dev' }, [{ status: 'done' }, { status: 'failed' }], TEAM],
  ['the live run is found past settled ones', { status: 'in_progress', assignee: 'a-dev' }, [{ status: 'done' }, { status: 'queued' }], TEAM],
  ['an agent off this board still reads as itself', { status: 'in_progress', assignee: 'a-stranger' }, [], TEAM],
];

/** `[name, issue, draft, team]` for the mention half. */
const mentionCases = [
  ['a staffed card takes no handover', { assignee: 'a-dev' }, 'hey @lead', TEAM],
  ['no mention at all', {}, 'just a comment', TEAM],
  ['an unknown handle staffs nobody', {}, '@nobody take this', TEAM],
  ['the first mention wins', {}, '@lead and @dev-1', TEAM],
  ['a mention at the start', {}, '@dev-1 please look', TEAM],
  ['a mention after whitespace', {}, 'please @dev-1 look', TEAM],
  ['a mention in parens', {}, '(cc @dev-1)', TEAM],
  ['an email is not a mention', {}, 'mail me@dev-1.com', TEAM],
  ['a trailing hyphen is trimmed', {}, 'ask @dev-1- about it', TEAM],
  ['a blocked card records the mention and staffs nobody', { blocked_reason: 'why' }, '@dev-1 help', TEAM],
];

const vectors = {
  comment: commentCases.map(([name, issue, runs, team]) => ({
    name,
    issue,
    runs,
    team,
    hint: commentHint(issue, runs, team),
  })),
  mention: mentionCases.map(([name, issue, draft, team]) => ({
    name,
    issue,
    draft,
    team,
    hint: mentionHint(issue, draft, team),
  })),
};

const out = join(
  dirname(fileURLToPath(import.meta.url)),
  '../src/pages/projects/commentHintVectors.json',
);
writeFileSync(out, `${JSON.stringify(vectors, null, 2)}\n`);
console.log(
  `wrote ${vectors.comment.length} comment + ${vectors.mention.length} mention vectors → ${out}`,
);
