import { useEffect, useState } from 'react';
import { Milkdown, MilkdownProvider, useEditor, useInstance } from '@milkdown/react';
import { defaultValueCtx, Editor, editorViewOptionsCtx, rootCtx } from '@milkdown/kit/core';
import { commonmark } from '@milkdown/kit/preset/commonmark';
import { gfm } from '@milkdown/kit/preset/gfm';
import { history } from '@milkdown/kit/plugin/history';
import { listener, listenerCtx } from '@milkdown/kit/plugin/listener';
import { math } from '@milkdown/plugin-math';
import { replaceAll } from '@milkdown/kit/utils';

/// A what-you-see markdown editor, shared by the card's description and the
/// comment composer so the two behave identically.
///
/// **The document is re-serialised on every edit.** Milkdown parses the
/// markdown into a ProseMirror document and writes a fresh one back out, so
/// list markers, emphasis characters and blank lines come back in the
/// serialiser's preferred spelling rather than yours — and the description is
/// pasted verbatim into the agent's brief (`crates/project/src/brief.rs`).
/// Nothing here can prevent that; what it does prevent is the *silent* case:
/// `onChange` only ever fires for a real edit, so opening a card and leaving
/// it never rewrites what the agent will read.

export type MarkdownEditorProps = {
  /// The markdown to open on. Read once, at mount — this is an editor, not a
  /// controlled input, and feeding it back its own output on every keystroke
  /// would fight the cursor.
  initialValue: string;
  onChange: (markdown: string) => void;
  onBlur?: () => void;
  placeholder?: string;
  ariaLabel: string;
  className?: string;
  /// Bumped by the parent to empty the editor — how the composer clears
  /// itself after a comment is sent.
  resetSignal?: number;
};

function Editing({
  initialValue,
  onChange,
  onBlur,
  placeholder,
  ariaLabel,
  className,
  resetSignal = 0,
}: MarkdownEditorProps) {
  // Whether to show the prompt, decided here rather than in CSS. An "empty"
  // ProseMirror paragraph is not `:empty` — it holds a trailing `<br>` the
  // editor puts there to keep the line selectable — so the obvious
  // `p:empty` rule matches nothing, ever.
  const [empty, setEmpty] = useState(initialValue.trim() === '');
  useEditor(
    (root) =>
      Editor.make()
        .config((ctx) => {
          ctx.set(rootCtx, root);
          ctx.set(defaultValueCtx, initialValue);
          ctx.update(editorViewOptionsCtx, (prev) => ({
            ...prev,
            attributes: {
              ...prev.attributes,
              'aria-label': ariaLabel,
              ...(placeholder === undefined ? {} : { 'data-placeholder': placeholder }),
            },
          }));
          ctx.get(listenerCtx).markdownUpdated((_ctx, markdown, previous) => {
            if (markdown === previous) return;
            setEmpty(markdown.trim() === '');
            onChange(markdown);
          });
          if (onBlur !== undefined) ctx.get(listenerCtx).blur(() => {
            onBlur();
          });
        })
        .use(commonmark)
        .use(gfm)
        .use(history)
        .use(math)
        .use(listener),
    [],
  );

  const [, getInstance] = useInstance();
  useEffect(() => {
    if (resetSignal === 0) return;
    getInstance()?.action(replaceAll(''));
    setEmpty(true);
  }, [resetSignal, getInstance]);

  return (
    <div className={`${className ?? ''} ${empty ? 'md-empty' : ''}`}>
      <Milkdown />
    </div>
  );
}

export function MarkdownEditor(props: MarkdownEditorProps) {
  return (
    <MilkdownProvider>
      <Editing {...props} />
    </MilkdownProvider>
  );
}
