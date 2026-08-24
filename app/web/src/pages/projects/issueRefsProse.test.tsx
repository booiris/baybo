import { describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { Schema } from '@milkdown/kit/prose/model';

import type { Issue } from './boardModel';
import { MarkdownEditor } from './MarkdownEditor';
import { issueRefRanges, useIssueRefPlugins } from './issueRefsProse';

// A stand-in for the description editor's schema: the two marks whose text
// already means something, one node ProseMirror flags as code, and nothing
// else. The real ids cannot be read here — `linkSchema.id` is `undefined`
// until Milkdown has run the plugin against a context — which is why the walk
// takes mark *types* rather than names.
const schema = new Schema({
  nodes: {
    doc: { content: 'block+' },
    paragraph: { group: 'block', content: 'inline*' },
    code_block: { group: 'block', content: 'text*', code: true, marks: '' },
    text: { group: 'inline' },
  },
  marks: { inlineCode: {}, link: {}, strong: {} },
});

const MEANS_SOMETHING_ELSE = new Set([schema.marks.inlineCode, schema.marks.link]);

const BOARD = new Map([
  [3, 'a card'],
  [12, 'Fix the reconnect storm'],
]);

function ranges(node: ReturnType<typeof schema.node>) {
  return issueRefRanges(node, BOARD, MEANS_SOMETHING_ELSE);
}

function paragraph(...inline: ReturnType<typeof schema.text>[]) {
  return schema.node('paragraph', null, inline);
}

function doc(...blocks: ReturnType<typeof schema.node>[]) {
  return schema.node('doc', null, blocks);
}

describe('issueRefRanges', () => {
  it('finds a reference where it sits in the document', () => {
    expect(ranges(doc(paragraph(schema.text('see #12 now'))))).toEqual([
      { from: 5, to: 8, number: 12 },
    ]);
  });

  it('counts through the block above it', () => {
    expect(ranges(doc(paragraph(schema.text('one')), paragraph(schema.text('#12'))))).toEqual([
      { from: 6, to: 9, number: 12 },
    ]);
  });

  it('finds every reference in a line', () => {
    expect(ranges(doc(paragraph(schema.text('#3 与 #12 都要'))))).toEqual([
      { from: 1, to: 3, number: 3 },
      { from: 6, to: 9, number: 12 },
    ]);
  });

  it('leaves a number this board does not have', () => {
    expect(ranges(doc(paragraph(schema.text('see #99 now'))))).toEqual([]);
  });

  it('leaves code alone, inline and in a block', () => {
    const inline = doc(
      paragraph(schema.text('see '), schema.text('#12', [schema.marks.inlineCode.create()])),
    );
    expect(ranges(inline)).toEqual([]);
    expect(ranges(doc(schema.node('code_block', null, schema.text('#12'))))).toEqual([]);
  });

  it('leaves the text of a link, which already points somewhere', () => {
    expect(ranges(doc(paragraph(schema.text('#12', [schema.marks.link.create()]))))).toEqual([]);
  });

  it('reads the run word out of a neighbouring mark', () => {
    const emphasised = doc(
      paragraph(schema.text('run', [schema.marks.strong.create()]), schema.text(' #3 failed')),
    );
    expect(ranges(emphasised)).toEqual([]);
  });
});

const PROJECT = 'p1';
const ISSUE_TITLE = 'Fix the reconnect storm';

function issue(number: number): Issue {
  return {
    number,
    project_id: PROJECT,
    title: ISSUE_TITLE,
    description: '',
    status: 'todo',
    priority: 'none',
    position: 0,
    pinned: false,
    stage: 1,
    unread: 0,
    last_run_failed: false,
    opened_by_agent: false,
    created_at_ms: 0,
    updated_at_ms: 0,
  };
}

function Where() {
  const location = useLocation();
  return (
    <span data-testid="where">
      {location.pathname} ← {String((location.state as { from?: unknown } | null)?.from)}
    </span>
  );
}

const READING = '/start';

function Description({ text, board }: { text: string; board: Issue[] }) {
  const plugins = useIssueRefPlugins(PROJECT, board);
  return (
    <MarkdownEditor
      initialValue={text}
      onChange={() => undefined}
      ariaLabel="Issue description"
      plugins={plugins}
    />
  );
}

function tree(text: string, board: Issue[]) {
  return (
    <MemoryRouter initialEntries={['/start']}>
      <Where />
      <Routes>
        <Route path="/start" element={<Description text={text} board={board} />} />
        <Route path="*" element={<span>elsewhere</span>} />
      </Routes>
    </MemoryRouter>
  );
}

function description(text: string, board: Issue[] = [issue(12)]) {
  return render(tree(text, board));
}

function click(node: Element, modified: boolean) {
  node.dispatchEvent(new MouseEvent('click', { bubbles: true, metaKey: modified }));
}

// The ranges above are the rule; this is the only thing that can see whether
// the plugin reached the editor at all. Milkdown really does mount under jsdom
// — the suite otherwise mocks `MarkdownEditor` away, so nothing else here has
// ever rendered a ProseMirror document.
describe('a description’s card references', () => {
  it('marks the card this board has, and only that one', async () => {
    const { container } = description('see #12 and #99 now');
    await waitFor(() => {
      expect(container.querySelector('[data-issue-number]')).not.toBeNull();
    });
    const marked = container.querySelectorAll('[data-issue-number]');
    expect(marked).toHaveLength(1);
    expect(marked[0].textContent).toBe('#12');
    expect(marked[0].getAttribute('title')).toContain(ISSUE_TITLE);
  });

  it('marks a card that only arrives after the description is on screen', async () => {
    const { container, rerender } = description('see #12 now', []);
    await waitFor(() => {
      expect(container.querySelector('.ProseMirror')).not.toBeNull();
    });
    expect(container.querySelector('[data-issue-number]')).toBeNull();

    rerender(tree('see #12 now', [issue(12)]));
    await waitFor(() => {
      expect(container.querySelector('[data-issue-number]')?.textContent).toBe('#12');
    });
  });

  it('opens the card on a modified click, and leaves a plain one to the caret', async () => {
    const { container } = description('see #12 now');
    await waitFor(() => {
      expect(container.querySelector('[data-issue-number]')).not.toBeNull();
    });
    const marked = container.querySelector('[data-issue-number]');
    if (marked === null) throw new Error('nothing was marked');

    click(marked, false);
    expect(screen.getByTestId('where').textContent).toBe(`${READING} ← undefined`);

    // And the card it opens is told where it came from, so its own back door
    // is this description rather than the board.
    click(marked, true);
    await waitFor(() => {
      expect(screen.getByTestId('where').textContent).toBe(
        `/projects/${PROJECT}/issues/12 ← ${READING}`,
      );
    });
  });
});
