import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';

import { MarkdownBody } from '../ChatPage';
import type { Issue } from './boardModel';
import { IssueRefScope, splitIssueRefs } from './issueRefs';

/// What the rule found, as the numbers alone — the rest of a piece list is the
/// prose it came from.
function refs(text: string, before?: string): number[] {
  return splitIssueRefs(text, before)
    .map((piece) => piece.number)
    .filter((number): number is number => number !== null);
}

describe('splitIssueRefs', () => {
  it('reads a card out of ordinary prose', () => {
    expect(refs('see #12 for the rest')).toEqual([12]);
    expect(refs('#12 is done')).toEqual([12]);
    expect(refs('closes #4 and #7')).toEqual([4, 7]);
  });

  it('reads one out of Chinese, which has no space to lean on', () => {
    expect(refs('#12的进度怎么样')).toEqual([12]);
    expect(refs('依赖#4 已解决')).toEqual([4]);
    expect(refs('#4、#7、#10 待跑')).toEqual([4, 7, 10]);
    expect(refs('结论：#12 已合并')).toEqual([12]);
  });

  it('keeps the prose around a reference intact', () => {
    expect(splitIssueRefs('see #12 now')).toEqual([
      { text: 'see ', number: null },
      { text: '#12', number: 12 },
      { text: ' now', number: null },
    ]);
  });

  it('leaves a number that counts runs rather than cards', () => {
    expect(refs('run #3 failed, see #12')).toEqual([12]);
    expect(refs('runs #1 and #2 both failed')).toEqual([]);
    expect(refs('run #3 和 #4 都失败了')).toEqual([]);
    expect(refs('run #3/#4 中断')).toEqual([]);
    expect(refs('This run (#3) was on #7')).toEqual([7]);
    expect(refs('重跑 #4 后仍失败')).toEqual([]);
    expect(refs('opened PR #123')).toEqual([]);
    expect(refs('attempt #2 timed out')).toEqual([]);
  });

  it('does not take a word that merely ends in one of those', () => {
    expect(refs('the overrun #12 case')).toEqual([12]);
  });

  it('leaves what is not a card number', () => {
    expect(refs('color #0d1117 and #fff')).toEqual([]);
    expect(refs('#007bff is the blue')).toEqual([]);
    expect(refs('see issue #130366 upstream')).toEqual([]);
    expect(refs('FIXME(rust-lang/rust#65991)')).toEqual([]);
    expect(refs('a &#39; entity')).toEqual([]);
    expect(refs('##3 is not one')).toEqual([]);
    expect(refs('#12beta')).toEqual([]);
  });

  it('reads the prose to its left, which is not always in the same breath', () => {
    expect(refs(' #3 失败', 'run')).toEqual([]);
    expect(refs('#12', 'rust-analyzer')).toEqual([]);
    expect(refs('#12', 'closes ')).toEqual([12]);
  });
});

const ISSUE_TITLE = 'Fix the reconnect storm';
const PROJECT = '01KZ85BA2R7HZ6ZNY4N6RSV52N';

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
    created_at_ms: 0,
    updated_at_ms: 0,
  };
}

const READING = `/projects/${PROJECT}/issues/7`;

function comment(text: string, board: Issue[] = [issue(12)]) {
  return render(
    <MemoryRouter initialEntries={[READING]}>
      <Routes>
        <Route
          path="/projects/:pid/issues/:num"
          element={
            <IssueRefScope projectId={PROJECT} issues={board}>
              <div data-testid="body">
                <MarkdownBody text={text} />
              </div>
              <Landed />
            </IssueRefScope>
          }
        />
      </Routes>
    </MemoryRouter>,
  );
}

/// Where the router is, and what the navigation that took it there recorded.
function Landed() {
  const location = useLocation();
  return (
    <span data-testid="landed">
      {location.pathname} ← {String((location.state as { from?: unknown } | null)?.from)}
    </span>
  );
}

// The pure rule above cannot see whether the plugin is attached to
// <ReactMarkdown> or whether the element it emits has a renderer — and
// `MarkdownFallback` catches anything thrown in there and re-renders the raw
// source, so a wholly broken pipeline is a silent, passing no-op. Every case
// checks `.md-failed` is absent for that reason.
describe('a comment’s card references', () => {
  it('links a card on this board, and names it', () => {
    const { container } = comment('see #12 for the rest');
    const link = container.querySelector('a');
    expect(container.querySelector('.md-failed')).toBeNull();
    // The `#` the app's HashRouter prefixes is its business, not the link's.
    expect(link?.getAttribute('href')).toBe(`/projects/${PROJECT}/issues/12`);
    expect(link?.textContent).toBe('#12');
    expect(link?.getAttribute('title')).toBe(ISSUE_TITLE);
  });

  it('tells the card it opens where it was opened from', async () => {
    // Otherwise the card you land on offers "← Board", and following a
    // reference costs you the card you were reading.
    const { container } = comment('see #12 for the rest');
    const link = container.querySelector('a');
    if (link === null) throw new Error('the reference did not render as a link');
    await userEvent.click(link);
    expect(screen.getByTestId('landed').textContent).toBe(
      `/projects/${PROJECT}/issues/12 ← ${READING}`,
    );
  });

  it('leaves a number this board does not have as text', () => {
    const { container } = comment('see #99 for the rest');
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('a')).toBeNull();
    expect(screen.getByTestId('body').textContent).toBe('see #99 for the rest');
  });

  it('leaves code alone', () => {
    const { container } = comment('run `fix #12` now');
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('a')).toBeNull();
    expect(container.querySelector('code')?.textContent).toBe('fix #12');
  });

  it('does not put a card inside a URL that ends in one', () => {
    const { container } = comment('see https://ex.com/a#12 here');
    expect(container.querySelector('.md-failed')).toBeNull();
    const links = [...container.querySelectorAll('a')];
    expect(links).toHaveLength(1);
    expect(links[0].getAttribute('href')).toBe('https://ex.com/a#12');
    expect(links[0].querySelector('a')).toBeNull();
  });

  it('opens a paragraph with one — `#12` at a line start is no heading', () => {
    const { container } = comment('#12 至此闭环');
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('h1')).toBeNull();
    expect(container.querySelector('a')?.textContent).toBe('#12');
  });

  it('leaves an escaped one as the characters the author asked for', () => {
    const { container } = comment('CSS 用 \\#12 做背景');
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('a')).toBeNull();
    expect(screen.getByTestId('body').textContent).toBe('CSS 用 #12 做背景');
  });

  it('reads the run word out of a neighbouring emphasis', () => {
    const { container } = comment('**run** #12 failed');
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('a')).toBeNull();
  });

  it('stays plain text where no board is in scope', () => {
    const { container } = render(<MarkdownBody text={'see #12 for the rest'} />);
    expect(container.querySelector('.md-failed')).toBeNull();
    expect(container.querySelector('a')).toBeNull();
    expect(container.textContent).toBe('see #12 for the rest');
  });
});
