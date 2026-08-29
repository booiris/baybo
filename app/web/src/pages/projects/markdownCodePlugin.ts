import { codeBlockSchema } from '@milkdown/kit/preset/commonmark';
import type { Node as ProseMirrorNode } from '@milkdown/kit/prose/model';
import { Plugin, PluginKey } from '@milkdown/kit/prose/state';
import { Decoration, DecorationSet, type NodeView } from '@milkdown/kit/prose/view';
import { $prose, $view } from '@milkdown/kit/utils';

import {
  CODE_COPY_FEEDBACK_MS,
  copyText,
  highlightCode,
} from '../../components/MarkdownCodeBlock';

const COPY_ICON =
  '<svg viewBox="0 0 20 20" aria-hidden="true"><rect x="6.5" y="6.5" width="9" height="9" rx="1.5"></rect><path d="M4.5 13.5h-1v-10h10v1"></path></svg>';
const CHECK_ICON =
  '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="m4.5 10.5 3.2 3.2 7.8-8"></path></svg>';

type TokenRange = { from: number; to: number; classes: string };

function tokenRanges(html: string): TokenRange[] {
  const template = document.createElement('template');
  template.innerHTML = html;
  const ranges: TokenRange[] = [];
  let offset = 0;

  const walk = (node: Node, inherited: string[]) => {
    if (node.nodeType === Node.TEXT_NODE) {
      const length = node.textContent?.length ?? 0;
      if (length > 0 && inherited.length > 0) {
        ranges.push({ from: offset, to: offset + length, classes: inherited.join(' ') });
      }
      offset += length;
      return;
    }
    const own =
      node instanceof HTMLElement
        ? [...node.classList].filter((className) => className.startsWith('hljs-'))
        : [];
    const classes = own.length === 0 ? inherited : [...inherited, ...own];
    for (const child of node.childNodes) walk(child, classes);
  };

  for (const child of template.content.childNodes) walk(child, []);
  return ranges;
}

function decorationsFor(doc: ProseMirrorNode): DecorationSet {
  const decorations: Decoration[] = [];
  doc.descendants((node, position) => {
    if (node.type.name !== 'code_block' || node.textContent === '') return;
    const language = typeof node.attrs.language === 'string' ? node.attrs.language : null;
    const highlighted = highlightCode(node.textContent, language);
    if (highlighted === null) return;
    for (const range of tokenRanges(highlighted.html)) {
      decorations.push(
        Decoration.inline(position + 1 + range.from, position + 1 + range.to, {
          class: range.classes,
        }),
      );
    }
  });
  return DecorationSet.create(doc, decorations);
}

const highlightKey = new PluginKey<DecorationSet>('markdown-code-highlight');

const codeHighlight = $prose(
  () =>
    new Plugin<DecorationSet>({
      key: highlightKey,
      state: {
        init: (_config, state) => decorationsFor(state.doc),
        apply: (transaction, current) =>
          transaction.docChanged
            ? decorationsFor(transaction.doc)
            : current.map(transaction.mapping, transaction.doc),
      },
      props: {
        decorations: (state) => highlightKey.getState(state) ?? DecorationSet.empty,
      },
    }),
);

function codeBlockView(initialNode: ProseMirrorNode): NodeView {
  let node = initialNode;
  let resetTimer: number | null = null;
  const dom = document.createElement('div');
  const button = document.createElement('button');
  const pre = document.createElement('pre');
  const code = document.createElement('code');

  dom.className = 'md-editor-code-block';
  button.type = 'button';
  button.className = 'md-code-copy';
  button.contentEditable = 'false';
  pre.append(code);
  dom.append(button, pre);

  const showCopied = (copied: boolean) => {
    button.innerHTML = copied ? CHECK_ICON : COPY_ICON;
    button.title = copied ? 'Copied' : 'Copy code';
    button.setAttribute('aria-label', copied ? 'Copied' : 'Copy code');
  };
  const syncLanguage = () => {
    const language = typeof node.attrs.language === 'string' ? node.attrs.language : '';
    if (language === '') delete pre.dataset.language;
    else pre.dataset.language = language;
  };
  const onPointerDown = (event: PointerEvent) => event.preventDefault();
  const onClick = () => {
    void copyText(node.textContent).then((didCopy) => {
      if (!didCopy) return;
      showCopied(true);
      if (resetTimer !== null) window.clearTimeout(resetTimer);
      resetTimer = window.setTimeout(() => showCopied(false), CODE_COPY_FEEDBACK_MS);
    });
  };

  showCopied(false);
  syncLanguage();
  button.addEventListener('pointerdown', onPointerDown);
  button.addEventListener('click', onClick);

  return {
    dom,
    contentDOM: code,
    update: (nextNode) => {
      if (nextNode.type !== node.type) return false;
      node = nextNode;
      syncLanguage();
      showCopied(false);
      return true;
    },
    stopEvent: (event) => event.target instanceof Node && button.contains(event.target),
    destroy: () => {
      if (resetTimer !== null) window.clearTimeout(resetTimer);
      button.removeEventListener('pointerdown', onPointerDown);
      button.removeEventListener('click', onClick);
    },
  };
}

const codeBlockChrome = $view(codeBlockSchema.node, () => (node) => codeBlockView(node));

export const markdownCodePlugins = [codeBlockChrome, codeHighlight];
