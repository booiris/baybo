import { useEffect, useMemo, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import type { MilkdownPlugin } from '@milkdown/kit/ctx';
import { inlineCodeSchema, linkSchema } from '@milkdown/kit/preset/commonmark';
import type { MarkType, Node as ProseNode } from '@milkdown/kit/prose/model';
import { Plugin, PluginKey } from '@milkdown/kit/prose/state';
import { Decoration, DecorationSet } from '@milkdown/kit/prose/view';
import { $prose } from '@milkdown/kit/utils';

import { issuePath, type Issue } from './boardModel';
import { ISSUE_REF_NUMBER, splitIssueRefs } from './issueRefs';

/// The card references in a description, as decorations rather than as anything
/// in the document.
///
/// The editor is the card's description itself, and it **re-serialises the whole
/// document on every edit** into the text an agent is briefed with
/// (`MarkdownEditor.tsx` says so at length). A node or a mark for `#12` would
/// have to survive that round trip; a decoration is drawn over the document and
/// is never asked to write itself back, so the bytes the agent reads cannot
/// change because a reference was rendered.

/// The look of a live reference, and the one thing it has to teach: this box is
/// an editor, so a plain click belongs to the caret.
const ISSUE_REF_CLASS =
  'font-bold text-info underline decoration-dotted underline-offset-2 hover:opacity-80';
const ISSUE_REF_HINT = '⌘/Ctrl-click to open';

/// Where a reference in the editor points, and what happens when one is
/// followed. Held behind a ref: the editor is built once, at mount, so anything
/// it closes over directly would still be the board that was on screen then.
type EditorScope = {
  titles: ReadonlyMap<number, string>;
  open: (number: number) => void;
};

export type IssueRefRange = { from: number; to: number; number: number };

/// Every `#N` in a document that names a card this board has.
///
/// `meansSomethingElse` is the marks whose text already says something — inline
/// code, and a link, which a reference inside would nest inside. They arrive as
/// the editor's own `MarkType`s rather than as names: `linkSchema.id` reads
/// `undefined` until the plugin has been run against a context, so a set of
/// names built at import time silently matches nothing.
export function issueRefRanges(
  doc: ProseNode,
  titles: ReadonlyMap<number, string>,
  meansSomethingElse: ReadonlySet<MarkType>,
): IssueRefRange[] {
  const ranges: IssueRefRange[] = [];
  doc.descendants((node, pos) => {
    if (!node.isTextblock) return true;
    // A code block carries its text as a node ProseMirror flags, not as a mark.
    if (node.type.spec.code === true) return false;
    // The prose to a reference's left is not always in the same text node:
    // `**run** #3` splits the word that disqualifies the number away from it.
    let before = '';
    node.forEach((child, offset) => {
      const text = child.text;
      if (text === undefined) return;
      if (child.marks.some((mark) => meansSomethingElse.has(mark.type))) {
        before = text;
        return;
      }
      let at = pos + 1 + offset;
      for (const piece of splitIssueRefs(text, before)) {
        if (piece.number !== null && titles.has(piece.number)) {
          ranges.push({ from: at, to: at + piece.text.length, number: piece.number });
        }
        at += piece.text.length;
      }
      before = text;
    });
    return false;
  });
  return ranges;
}

function issueRefProse(
  scope: { current: EditorScope },
  redraw: { current: () => void },
): MilkdownPlugin {
  return $prose((ctx) => {
    const meansSomethingElse = new Set([inlineCodeSchema.type(ctx), linkSchema.type(ctx)]);
    return new Plugin({
      key: new PluginKey(ISSUE_REF_NUMBER),
      // Decorations are recomputed when the editor's state moves, and the board
      // arriving is not something the editor's state knows about — without a
      // nudge, a card created while this description sat untouched would stay
      // plain until the next keystroke.
      view: (editorView) => {
        redraw.current = () => {
          editorView.dispatch(editorView.state.tr);
        };
        return {
          destroy: () => {
            redraw.current = () => undefined;
          },
        };
      },
      props: {
        decorations: (state) =>
          DecorationSet.create(
            state.doc,
            issueRefRanges(state.doc, scope.current.titles, meansSomethingElse).map((range) =>
              Decoration.inline(range.from, range.to, {
                class: ISSUE_REF_CLASS,
                title: `${ISSUE_REF_HINT} · ${scope.current.titles.get(range.number) ?? ''}`,
                [ISSUE_REF_NUMBER]: String(range.number),
              }),
            ),
          ),
        // A plain click belongs to the caret — this box is an editor, and a
        // reference you cannot put the cursor into is one you cannot fix.
        handleDOMEvents: {
          click: (_view, event) => {
            if (!event.metaKey && !event.ctrlKey) return false;
            const target =
              event.target instanceof HTMLElement
                ? event.target.closest(`[${ISSUE_REF_NUMBER}]`)
                : null;
            const number = Number(target?.getAttribute(ISSUE_REF_NUMBER));
            if (Number.isNaN(number)) return false;
            event.preventDefault();
            scope.current.open(number);
            return true;
          },
        },
      },
    });
  });
}

/// The plugins a description editor needs to make its `#N` live.
///
/// The array is built once and never rebuilt: `MarkdownEditor` reads its
/// plugins at mount, and a fresh array would be read by nobody. What changes —
/// the board, and where a click goes — rides the ref.
export function useIssueRefPlugins(projectId: string, issues: Issue[]): MilkdownPlugin[] {
  const navigate = useNavigate();
  const titles = useMemo(
    () => new Map(issues.map((issue) => [issue.number, issue.title])),
    [issues],
  );
  const scope = useRef<EditorScope>({ titles, open: () => undefined });
  scope.current = {
    titles,
    open: (number) => {
      navigate(issuePath(projectId, number));
    },
  };
  const redraw = useRef<() => void>(() => undefined);
  useEffect(() => {
    redraw.current();
  }, [titles]);
  return useMemo(() => [issueRefProse(scope, redraw)], []);
}
