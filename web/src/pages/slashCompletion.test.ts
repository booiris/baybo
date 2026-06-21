import { describe, it, expect } from 'vitest';

import { applySlashCompletion, caretOnSlashToken } from './ChatPage';

// Pins slash-command Tab completion to the TUI's `completion_accept`
// (crates/tui/src/app.rs): the leading command token is replaced with the
// full `/name ` and the caret lands just after it, with any trailing args
// preserved.

describe('applySlashCompletion', () => {
  it('completes a partial command, adding a trailing space, caret after it', () => {
    // "/sto" -> "/stop " with caret at index 6 (just past the space).
    expect(applySlashCompletion('/sto', 'stop')).toEqual({ text: '/stop ', caret: 6 });
  });

  it('preserves trailing args and lands the caret before them', () => {
    expect(applySlashCompletion('/mod gpt-5', 'model')).toEqual({
      text: '/model gpt-5',
      caret: 7,
    });
  });

  it('collapses the whitespace between the command and its args to one space', () => {
    expect(applySlashCompletion('/m   foo bar', 'model')).toEqual({
      text: '/model foo bar',
      caret: 7,
    });
  });

  it('re-accepting an already-complete command is idempotent on the text', () => {
    expect(applySlashCompletion('/stop ', 'stop')).toEqual({ text: '/stop ', caret: 6 });
  });

  it('completes from a bare slash', () => {
    expect(applySlashCompletion('/', 'help')).toEqual({ text: '/help ', caret: 6 });
  });
});

describe('caretOnSlashToken', () => {
  it('is false for a non-slash draft', () => {
    expect(caretOnSlashToken('hello', 3)).toBe(false);
  });

  it('is true while the caret sits anywhere on the leading /command token', () => {
    expect(caretOnSlashToken('/model', 0)).toBe(true);
    expect(caretOnSlashToken('/model', 3)).toBe(true);
    expect(caretOnSlashToken('/model', 6)).toBe(true); // caret at token end
  });

  it('is true at the boundary (caret on the first space) and false past it', () => {
    // prefixEnd is the first-whitespace index (6); caret <= prefixEnd stays open.
    expect(caretOnSlashToken('/model gpt', 6)).toBe(true);
    expect(caretOnSlashToken('/model gpt', 7)).toBe(false);
    expect(caretOnSlashToken('/model gpt', 10)).toBe(false);
  });
});
