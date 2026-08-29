import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { downloadBlob } from '../../api/blobs';
import type { WireAttachment } from '../../api/chatWs';
import { AttachmentList } from './AttachmentList';

vi.mock('../../api/blobs', () => ({
  downloadBlob: vi.fn(),
}));
vi.mock('./AttachmentImage', () => ({
  AttachmentImage: ({ alt }: { alt: string }) => <div data-testid="image-attachment">{alt}</div>,
}));

const attachment: WireAttachment = {
  kind: 'file',
  blob_id: 'sha256:abc.def',
  mime_type: 'application/pdf',
  size: 2048,
  filename: 'report.pdf',
};

describe('AttachmentList', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('downloads a non-image attachment with the admin bearer', async () => {
    const user = userEvent.setup();
    render(
      <AttachmentList
        attachments={[attachment]}
        baseUrl="http://gw:8888"
        adminToken="admin-token"
      />,
    );

    const chip = screen.getByRole('button', { name: /report\.pdf/ });
    expect(chip).toHaveAttribute('title', expect.stringContaining('click to download'));
    await user.click(chip);

    expect(downloadBlob).toHaveBeenCalledWith(
      'http://gw:8888',
      'sha256:abc.def',
      'admin-token',
      'report.pdf',
    );
  });

  it('keeps image attachments on the preview path', () => {
    render(
      <AttachmentList
        attachments={[{ ...attachment, kind: 'image', mime_type: 'image/png', filename: 'shot.png' }]}
        baseUrl="http://gw:8888"
        adminToken="admin-token"
      />,
    );

    expect(screen.getByTestId('image-attachment')).toHaveTextContent('shot.png');
    expect(screen.queryByRole('button', { name: /shot\.png/ })).not.toBeInTheDocument();
  });
});
