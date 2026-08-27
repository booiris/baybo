import { createContext, useContext, useMemo, type ReactNode } from 'react';
import { Link, useLocation } from 'react-router-dom';

import { issuePath, type Issue } from './boardModel';

/// The element a parsed `#N` becomes on its way through the markdown pipeline,
/// and the property carrying the number it read.
///
/// A custom tag rather than a link node: the pipeline that parses a comment
/// does not know which board the comment belongs to, so a reference is only
/// *recognised* here and given its destination further down, by whoever mounted
/// [`IssueRefScope`].
export const ISSUE_REF_TAG = 'issue-ref';
/// Which card a marked reference names. Also what the description's editor
/// hangs off its decorations — the two surfaces mark the same thing, so they
/// spell it once (see `issueRefsProse.ts`).
export const ISSUE_REF_NUMBER = 'data-issue-number';

// TypeScript takes only a literal as an interface key, so this one spelling of
// the tag cannot come from the const above.
declare module 'react' {
  namespace JSX {
    interface IntrinsicElements {
      'issue-ref': { children?: ReactNode; 'data-issue-number'?: string };
    }
  }
}

/// What a card reference may look like.
///
/// The left edge takes anything that is not a word character, which is wider
/// than the `@handle` rule beside it — `mentionModel` demands whitespace or
/// `(`, and this board is worked in Chinese, where `依赖#4` and `#3、#4` have no
/// space to separate on. `&` and `#` are held back so `&#39;` and `##3` are not
/// references.
///
/// The number may not lead with a zero and may not run past five digits, which
/// is what keeps `#0d1117` and an upstream `#130366` out: five digits followed
/// by a sixth fails the right edge at every length the counter can back off to,
/// so a long number produces no match at all rather than a truncated one. A
/// digits-only CSS colour (`#333`) is indistinguishable from a card and is left
/// to the existence check — on a board with 333 cards it does get through, and
/// backticks are the way to write one that must not.
const REFERENCE = /(^|[^0-9A-Za-z_&#])#([1-9][0-9]{0,4})(?![0-9A-Za-z_])/g;

/// What a `#N` is counting when it is not counting cards.
///
/// Older board prose and `IssueGet` write a run's ordinal as `run #3`, which
/// agents can pick up in comments even though the project UI now renders that
/// ordinal as `turn 3`. On a board with three cards, legacy `run #3` and card
/// `#3` are the same three characters. The trailing clause is for
/// `run #3/#4` and `runs #1 and #2`, where only the first number wears the
/// word. A `pr` is somebody else's forge, and cannot be resolved here at all.
const COUNTING_SOMETHING_ELSE =
  /(?:^|[^0-9A-Za-z_])(?:runs?|attempts?|prs?|pull requests?|重跑|重试|再跑)\s*[(（]?\s*(?:#[1-9][0-9]{0,4}\s*(?:[/,、+&;；–—]|and|和|与|及)\s*)*$/i;

/// How much of what precedes a stretch of text is worth reading for the two
/// rules above. Long enough for `runs #1 and #2 and #3`, short enough that a
/// paragraph is not rescanned per text node.
const CONTEXT_CHARS = 64;

/// One run of text, and the card it points at if it is a reference.
export type IssueRefPiece = { text: string; number: number | null };

/// Cut a run of prose into its plain stretches and its card references.
///
/// `before` is the prose immediately to the left, which in a parsed document is
/// not always in the same node: `**run** #3` puts the word that disqualifies
/// the number in a sibling, and `` `rust-analyzer`#12 `` puts the character
/// that would have failed the left edge in one.
export function splitIssueRefs(text: string, before = ''): IssueRefPiece[] {
  const context = before.slice(-CONTEXT_CHARS);
  const scanned = `${context}${text}`;
  const pieces: IssueRefPiece[] = [];
  let plain = 0;
  REFERENCE.lastIndex = 0;
  for (let hit = REFERENCE.exec(scanned); hit !== null; hit = REFERENCE.exec(scanned)) {
    const at = hit.index + hit[1].length - context.length;
    if (at < 0) continue;
    if (COUNTING_SOMETHING_ELSE.test(scanned.slice(0, at + context.length))) continue;
    if (at > plain) pieces.push({ text: text.slice(plain, at), number: null });
    const reference = `#${hit[2]}`;
    pieces.push({ text: reference, number: Number(hit[2]) });
    plain = at + reference.length;
  }
  if (pieces.length === 0) return [{ text, number: null }];
  if (plain < text.length) pieces.push({ text: text.slice(plain), number: null });
  return pieces;
}

/// The shape of the parsed document this walks. `@types/mdast` is not reachable
/// from this workspace (pnpm does not hoist it) and the walk needs four fields.
type MarkdownNode = {
  type: string;
  value?: string;
  children?: MarkdownNode[];
  position?: { start: { offset?: number }; end: { offset?: number } };
  data?: { hName?: string; hProperties?: Record<string, string> };
};

/// Text inside these already points somewhere, and a reference nested in a link
/// is a link inside a link. Not academic: `remark-gfm` autolinks a bare URL, so
/// `https://x.dev/a#12` arrives here as link-wrapped text carrying what looks
/// exactly like a reference. Code needs no such guard — a code span holds a
/// value rather than text children, so the walk never reaches into one.
const ALREADY_POINTS_SOMEWHERE = new Set(['link', 'linkReference', 'definition']);

/// The text a node contributes to the prose, for the left context of whatever
/// follows it. Code counts: it is what the reader sees.
function proseOf(node: MarkdownNode): string {
  if (node.value !== undefined) return node.value;
  return (node.children ?? []).map(proseOf).join('');
}

/// Whether the source this node was parsed from is longer than the text it
/// carries — which means the author escaped something in it (`\#333`) or wrote
/// an entity, and the parser has already eaten the evidence. Nothing in the
/// node is linked in that case: `\#333` is a reader asking for the characters,
/// and there is no way to tell it from `#333` by value alone.
function holdsAnEscape(node: MarkdownNode): boolean {
  const start = node.position?.start.offset;
  const end = node.position?.end.offset;
  if (start === undefined || end === undefined) return false;
  return end - start !== (node.value?.length ?? 0);
}

function mark(node: MarkdownNode): void {
  const children = node.children;
  if (children === undefined || ALREADY_POINTS_SOMEWHERE.has(node.type)) return;
  const marked: MarkdownNode[] = [];
  let found = false;
  let before = '';
  for (const child of children) {
    if (child.type !== 'text' || child.value === undefined || holdsAnEscape(child)) {
      mark(child);
      marked.push(child);
      before = proseOf(child);
      continue;
    }
    const pieces = splitIssueRefs(child.value, before);
    before = child.value;
    if (pieces.every((piece) => piece.number === null)) {
      marked.push(child);
      continue;
    }
    found = true;
    for (const piece of pieces) {
      marked.push(
        piece.number === null
          ? { type: 'text', value: piece.text }
          : {
              type: 'issueRef',
              children: [{ type: 'text', value: piece.text }],
              data: {
                hName: ISSUE_REF_TAG,
                hProperties: { [ISSUE_REF_NUMBER]: String(piece.number) },
              },
            },
      );
    }
  }
  if (found) node.children = marked;
}

/// Mark every `#N` in a document's prose. Parameterless and module-level: the
/// plugin list it joins is shared by every markdown surface in the app, and a
/// board handed in here would rebuild that list — and re-parse every message —
/// on each render.
export function remarkIssueRefs(): (tree: MarkdownNode) => void {
  return (tree) => {
    mark(tree);
  };
}

/// Where a `#N` resolves, for the subtree that has a board to resolve it on —
/// and the page it is being read on, which is what the card it opens comes
/// back to.
type Scope = { projectId: string; titles: ReadonlyMap<number, string>; here: string };

const ScopeContext = createContext<Scope | null>(null);

/// Give the prose under here a board to read its `#N` against.
///
/// Context rather than a prop on the renderer: `MarkdownBody` is memoized on
/// its props and shared with chat, so a board threaded through it would defeat
/// that memo and hand chat a concept it has no use for. Nothing mounts this
/// outside a project page, and prose that renders without it keeps its `#N` as
/// text.
export function IssueRefScope({
  projectId,
  issues,
  children,
}: {
  projectId: string;
  issues: Issue[];
  children: ReactNode;
}) {
  const here = useLocation().pathname;
  const scope = useMemo(
    () => ({
      projectId,
      titles: new Map(issues.map((issue) => [issue.number, issue.title])),
      here,
    }),
    [projectId, issues, here],
  );
  return <ScopeContext.Provider value={scope}>{children}</ScopeContext.Provider>;
}

/// A reference, once it is known to name a card on this board.
///
/// A number this board does not have stays text — the same bargain the composer
/// strikes with an unknown `@handle`, and what keeps a quoted upstream issue, a
/// colour, or a number that only looks like one from becoming a link to a
/// not-found page. Deliberately not a dimmed or struck variant: until the board
/// has answered, "no such card" and "not loaded yet" look the same from here,
/// and only plain text asserts neither. The dotted rule is what tells a card
/// apart from a URL at a glance.
function IssueRef({
  children,
  [ISSUE_REF_NUMBER]: raw,
}: { children?: ReactNode } & Partial<Record<typeof ISSUE_REF_NUMBER, string>>) {
  const scope = useContext(ScopeContext);
  const number = raw === undefined ? Number.NaN : Number(raw);
  const title = scope === null || Number.isNaN(number) ? undefined : scope.titles.get(number);
  if (scope === null || title === undefined) return <>{children}</>;
  return (
    <Link
      to={issuePath(scope.projectId, number)}
      state={{ from: scope.here }}
      title={title}
      className="font-bold text-info underline decoration-dotted underline-offset-2 hover:opacity-80"
    >
      {children}
    </Link>
  );
}

/// The renderer for [`ISSUE_REF_TAG`], for the shared component map.
export const ISSUE_REF_COMPONENTS = { [ISSUE_REF_TAG]: IssueRef };
