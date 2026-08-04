import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { AttachmentImage } from './AttachmentImage';
import { installObjectUrls, installPointerCapture } from '../../test/domGaps';

// Covers the thumbnail's two jobs: fetching the blob with the admin bearer
// (an <img> can't carry the header) and handing that one object URL to the
// full-screen viewer instead of downloading the blob a second time.

const OBJECT_URL = 'blob:object-url-1';

installObjectUrls(OBJECT_URL);
installPointerCapture();

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn(async () => ({ ok: true, status: 200, blob: async () => new Blob(['x']) }));
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function renderThumb() {
  return render(
    <AttachmentImage
      blobId="sha256:abc.def"
      alt="portrait.png"
      baseUrl="http://gw:8888"
      adminToken="tok"
    />,
  );
}

describe('AttachmentImage', () => {
  it('fetches the blob with the bearer and shows it as an object URL', async () => {
    renderThumb();
    const img = await screen.findByAltText('portrait.png');
    expect(img).toHaveAttribute('src', OBJECT_URL);
    expect(fetchMock).toHaveBeenCalledWith('http://gw:8888/v1/blobs/sha256%3Aabc.def', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('opens the viewer on the already-fetched URL, with no second fetch', async () => {
    const user = userEvent.setup();
    renderThumb();
    const img = await screen.findByAltText('portrait.png');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();

    await user.click(img);

    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveAttribute('aria-label', 'portrait.png');
    expect(screen.getByLabelText('Download image')).toHaveAttribute('href', OBJECT_URL);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    await user.click(screen.getByLabelText('Close'));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('falls back to a named chip when the blob fetch fails', async () => {
    fetchMock.mockResolvedValue({ ok: false, status: 404 });
    renderThumb();
    expect(await screen.findByText('portrait.png')).toBeInTheDocument();
    expect(screen.queryByAltText('portrait.png')).not.toBeInTheDocument();
  });
});
