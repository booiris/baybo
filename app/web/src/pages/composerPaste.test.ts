import { describe, it, expect } from 'vitest';

import { clipboardAttachments, pastedFilename, type PastedClipboard } from './ChatPage';

// Pins the paste-to-attach rule. The invariant that matters is the ASYMMETRY:
// a clipboard with files and no text becomes attachments (a copied screenshot,
// an image copied off a web page, a file copied in Finder), while a clipboard
// carrying real text stays an ordinary text paste EVEN IF a bitmap rides along
// — which is what a rich-text range copied out of Safari, Excel or Numbers
// looks like. Getting that backwards swallows the paste the user meant and
// attaches a picture of it instead.

function clipboard(
  entries: { kind: string; file: File | null }[],
  text = '',
): PastedClipboard {
  return {
    items: entries.map((entry) => ({ kind: entry.kind, getAsFile: () => entry.file })),
    getData: (type: string) => (type === 'text/plain' ? text : ''),
  };
}

function png(name = 'shot.png'): File {
  return new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], name, { type: 'image/png' });
}

describe('clipboardAttachments', () => {
  it('takes an image pasted with nothing else on the clipboard', () => {
    const file = png();
    expect(clipboardAttachments(clipboard([{ kind: 'file', file }]))).toEqual([file]);
  });

  it('takes several files, in clipboard order', () => {
    const first = png('a.png');
    const second = png('b.png');
    expect(
      clipboardAttachments(
        clipboard([
          { kind: 'file', file: first },
          { kind: 'file', file: second },
        ]),
      ),
    ).toEqual([first, second]);
  });

  it('takes a file whose clipboard entry carries no name at all', () => {
    const anonymous = new File([new Uint8Array([1])], '', { type: 'image/png' });
    expect(clipboardAttachments(clipboard([{ kind: 'file', file: anonymous }]))).toEqual([
      anonymous,
    ]);
  });

  it('ignores string entries — a plain text paste stages nothing', () => {
    expect(clipboardAttachments(clipboard([{ kind: 'string', file: null }], 'hello'))).toEqual([]);
  });

  it('stages nothing from an empty clipboard', () => {
    expect(clipboardAttachments(clipboard([]))).toEqual([]);
  });

  it('REGRESSION: real text wins over a bitmap of the same selection', () => {
    // Safari / Excel / Numbers put text/plain, text/html AND an image of the
    // copied range on the clipboard. Keying on "has a file" alone would drop
    // the text and attach the screenshot.
    expect(
      clipboardAttachments(
        clipboard(
          [
            { kind: 'string', file: null },
            { kind: 'file', file: png() },
          ],
          'A1\tB1',
        ),
      ),
    ).toEqual([]);
  });

  it('a file item that hands back nothing is skipped, not staged as null', () => {
    const file = png();
    expect(
      clipboardAttachments(
        clipboard([
          { kind: 'file', file: null },
          { kind: 'file', file },
        ]),
      ),
    ).toEqual([file]);
  });

  it('whitespace is still text — it falls through to the textarea', () => {
    // `getData('text/plain')` is not trimmed on purpose: a paste of a newline
    // is a text paste, and preventing its default would silently eat it.
    expect(clipboardAttachments(clipboard([{ kind: 'file', file: png() }], '\n'))).toEqual([]);
  });
});

describe('pastedFilename', () => {
  it('names the first pasted image off its mime subtype', () => {
    expect(pastedFilename('image/png', 0)).toBe('pasted-image.png');
    expect(pastedFilename('image/jpeg', 0)).toBe('pasted-image.jpeg');
    expect(pastedFilename('image/webp', 0)).toBe('pasted-image.webp');
  });

  it('numbers the rest of a multi-image paste from 2', () => {
    expect(pastedFilename('image/png', 1)).toBe('pasted-image-2.png');
    expect(pastedFilename('image/png', 2)).toBe('pasted-image-3.png');
  });

  it('drops a structured suffix rather than putting it in the extension', () => {
    expect(pastedFilename('image/svg+xml', 0)).toBe('pasted-image.svg');
  });

  it('leaves the name extensionless for a mime it cannot spell', () => {
    // No second mime table: anything that isn't a plain image/<word> gets no
    // extension rather than a made-up one.
    expect(pastedFilename('application/octet-stream', 0)).toBe('pasted-image');
    expect(pastedFilename('', 0)).toBe('pasted-image');
    expect(pastedFilename('image/x-my-format', 0)).toBe('pasted-image');
  });
});
