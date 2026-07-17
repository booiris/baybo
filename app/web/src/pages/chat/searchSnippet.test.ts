import { describe, expect, it } from 'vitest';

import { queryChunks, snippet, type Segment } from './searchSnippet';

const plain = (segs: Segment[]) => segs.map((s) => s.text).join('');
const matched = (segs: Segment[]) => segs.filter((s) => s.match).map((s) => s.text);

describe('queryChunks', () => {
  it('splits on whitespace, the way the server ANDs the query', () => {
    expect(queryChunks('数据库 迁移')).toEqual(['数据库', '迁移']);
    expect(queryChunks('  foo   bar  ')).toEqual(['foo', 'bar']);
  });

  it('drops chunks the server would tokenize to nothing', () => {
    expect(queryChunks('数据 --- 库')).toEqual(['数据', '库']);
    expect(queryChunks('---')).toEqual([]);
    expect(queryChunks('')).toEqual([]);
  });
});

describe('snippet', () => {
  it('highlights a single term and pads around it', () => {
    const segs = snippet('先说说数据库的事情', '数据库');
    expect(matched(segs)).toEqual(['数据库']);
    expect(plain(segs)).toBe('先说说数据库的事情');
  });

  // The whole point. The server searches each chunk independently and ANDs
  // them, so this message is a legitimate hit for `foo bar` while containing no
  // such substring. Looking for the literal input found nothing and fell back to
  // the head — an excerpt with none of what was searched for.
  it('anchors on a term when the query terms are far apart', () => {
    const text = `bar ${'x'.repeat(500)} foo`;
    const segs = snippet(text, 'foo bar');
    expect(matched(segs)).toEqual(['bar']);
    expect(plain(segs).startsWith('bar')).toBe(true);
    expect(plain(segs).endsWith('…')).toBe(true);
  });

  it('anchors on the EARLIEST term regardless of the order typed', () => {
    const text = `${'x'.repeat(200)}迁移${'y'.repeat(500)}数据库`;
    // 迁移 comes first in the text though 数据库 was typed first.
    expect(matched(snippet(text, '数据库 迁移'))).toEqual(['迁移']);
  });

  it('highlights every term that lands in the window', () => {
    const segs = snippet('聊聊数据库的迁移方案', '数据库 迁移');
    expect(matched(segs)).toEqual(['数据库', '迁移']);
    expect(plain(segs)).toBe('聊聊数据库的迁移方案');
  });

  it('highlights repeats of one term', () => {
    expect(matched(snippet('foo and foo again', 'foo'))).toEqual(['foo', 'foo']);
  });

  it('matches case-insensitively but shows the original casing', () => {
    const segs = snippet('The Session started', 'session');
    expect(matched(segs)).toEqual(['Session']);
    expect(plain(segs)).toBe('The Session started');
  });

  // The server widens a Latin chunk to `session*`, so `sessions` is a hit; the
  // query is a prefix of the word, so a substring search still locates it.
  it('locates a hit the server reached by prefix widening', () => {
    expect(matched(snippet('list the sessions please', 'session'))).toEqual(['session']);
  });

  it('elides both ends when the window is interior', () => {
    const text = `${'a'.repeat(200)}检索${'b'.repeat(200)}`;
    const segs = snippet(text, '检索');
    expect(plain(segs).startsWith('…')).toBe(true);
    expect(plain(segs).endsWith('…')).toBe(true);
    expect(matched(segs)).toEqual(['检索']);
  });

  // `unicode61` folds diacritics, so the server matches where a substring search
  // cannot. A head excerpt is the honest degradation — never an empty card.
  it('falls back to the head when no term can be located', () => {
    const segs = snippet('résumé of the work', 'resume');
    expect(matched(segs)).toEqual([]);
    expect(plain(segs)).toBe('résumé of the work');
  });

  it('never returns an empty excerpt', () => {
    for (const [text, query] of [
      ['', 'foo'],
      ['some text', ''],
      ['some text', '---'],
    ]) {
      expect(snippet(text, query)).toBeInstanceOf(Array);
    }
    expect(plain(snippet('some text', '---'))).toBe('some text');
  });
});
