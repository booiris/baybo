import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';

import { AttachmentList, AttachmentTray, attachmentName, isImage, readyRequests } from './Attachments';
import type { DraftAttachment, IssueAttachment } from './Attachments';

vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ token: 't', baseUrl: 'http://gw', logout: vi.fn() }),
}));

// The image arm fetches its bytes to build an object URL; these tests are
// about which arm is chosen, so the fetch never has to resolve.
const fetchMock = vi.fn(() => new Promise(() => undefined));
vi.stubGlobal('fetch', fetchMock);

function stored(overrides: Partial<IssueAttachment> = {}): IssueAttachment {
  return {
    blob_id: 'sha256:abc.def',
    mime_type: 'application/pdf',
    size: 2048,
    filename: 'report.pdf',
    ...overrides,
  };
}

describe('what the mime decides', () => {
  it('reads a type the way a content-type header actually arrives', () => {
    expect(isImage('image/png')).toBe(true);
    expect(isImage('IMAGE/PNG')).toBe(true);
    expect(isImage('application/pdf')).toBe(false);
    // Not `startsWith('image')` — the slash is part of the answer.
    expect(isImage('imagery/x')).toBe(false);
  });

  it('names a file that arrived without a name', () => {
    expect(attachmentName(stored({ filename: 'a.png' }))).toBe('a.png');
    expect(attachmentName(stored({ filename: undefined }))).toBe('attachment');
    expect(attachmentName(stored({ filename: '   ' }))).toBe('attachment');
  });
});

describe('what a draft sends', () => {
  const draft = (over: Partial<DraftAttachment>): DraftAttachment => ({
    localId: 'a1',
    filename: 'f.png',
    mimeType: 'image/png',
    size: 1,
    status: 'ready',
    ...over,
  });

  it('sends only the files that actually landed', () => {
    const requests = readyRequests([
      draft({ localId: 'a1', blobId: 'sha256:1.x' }),
      draft({ localId: 'a2', status: 'uploading' }),
      draft({ localId: 'a3', status: 'error' }),
      draft({ localId: 'a4', blobId: 'sha256:2.x', filename: 'b.pdf' }),
    ]);
    expect(requests).toEqual([
      { blob_id: 'sha256:1.x', filename: 'f.png' },
      { blob_id: 'sha256:2.x', filename: 'b.pdf' },
    ]);
  });

  it('drops a ready file that somehow has no id rather than sending an empty one', () => {
    expect(readyRequests([draft({ blobId: undefined })])).toEqual([]);
  });
});

describe('a stored file on the page', () => {
  it('gives a non-image a way to get the bytes back out', () => {
    render(<AttachmentList attachments={[stored()]} />);
    // The whole point: before this the dashboard could attach a PDF and then
    // offer no way at all to open it.
    const chip = screen.getByRole('button', { name: /report\.pdf/ });
    expect(chip).toHaveAttribute('title', expect.stringContaining('click to download'));
  });

  it('draws an image instead of naming it', () => {
    render(
      <AttachmentList attachments={[stored({ mime_type: 'image/png', filename: 'shot.png' })]} />,
    );
    expect(screen.queryByRole('button', { name: /shot\.png/ })).toBeNull();
  });

  it('gives a description’s non-image a way out too, not just a comment’s', () => {
    // The same failure this feature was built to avoid, one component over:
    // the description's chips are an EDIT widget, and the download lived only
    // in the timeline's read-only one — so a PDF on a description could be
    // attached and never opened.
    render(
      <AttachmentTray
        attachments={[
          {
            localId: 'a1',
            filename: 'spec.pdf',
            mimeType: 'application/pdf',
            size: 2048,
            status: 'ready',
            blobId: 'sha256:abc.def',
          },
        ]}
        onRemove={vi.fn()}
      />,
    );
    // Exact, not a regex: "Remove spec.pdf" is a button on the same chip.
    expect(screen.getByRole('button', { name: 'spec.pdf' })).toHaveAttribute(
      'title',
      expect.stringContaining('click to download'),
    );
    // And the ✕ is still its own control, not swallowed by it.
    expect(screen.getByRole('button', { name: 'Remove spec.pdf' })).toBeInTheDocument();
  });

  it('offers no download for a file that has not finished uploading', () => {
    render(
      <AttachmentTray
        attachments={[
          {
            localId: 'a1',
            filename: 'spec.pdf',
            mimeType: 'application/pdf',
            size: 2048,
            status: 'uploading',
          },
        ]}
        onRemove={vi.fn()}
      />,
    );
    expect(screen.queryByRole('button', { name: 'spec.pdf' })).toBeNull();
    // The ✕ is still there — a file whose upload is in flight can be dropped.
    expect(screen.getByRole('button', { name: 'Remove spec.pdf' })).toBeInTheDocument();
  });

  it('draws a picked image as a square rather than naming it, chat’s composer shape', () => {
    // Cleared so the "no round-trip" assertion below is about THIS render —
    // the mock is module-scoped and the image test above already spent one.
    fetchMock.mockClear();
    render(
      <AttachmentTray
        attachments={[
          {
            localId: 'a1',
            filename: 'shot.png',
            mimeType: 'image/png',
            size: 4096,
            status: 'ready',
            blobId: 'sha256:abc.def',
            previewUrl: 'blob:local/1',
          },
        ]}
        onRemove={vi.fn()}
      />,
    );
    // The picture identifies itself; the filename is no longer spending a
    // row's width repeating what the pixels already say.
    const img = screen.getByRole('img', { name: 'shot.png' });
    expect(img).toHaveAttribute('src', 'blob:local/1');
    expect(img.className).toContain('h-14');
    // Straight from the local file — no upload round-trip to show it.
    expect(fetchMock).not.toHaveBeenCalled();
    // The ✕ is still reachable, now on the corner.
    expect(screen.getByRole('button', { name: 'Remove shot.png' })).toBeInTheDocument();
  });

  it('renders nothing at all when a card has no files', () => {
    const { container } = render(<AttachmentList attachments={[]} />);
    expect(container).toBeEmptyDOMElement();
  });
});
