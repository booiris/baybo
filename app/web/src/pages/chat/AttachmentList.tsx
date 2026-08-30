import { RiDownloadLine, RiFileLine } from 'react-icons/ri';

import { downloadBlob } from '../../api/blobs';
import type { WireAttachment } from '../../api/chatWs';
import { AttachmentImage } from './AttachmentImage';

export function AttachmentList({
  attachments,
  baseUrl,
  adminToken,
}: {
  attachments: WireAttachment[];
  baseUrl: string;
  adminToken: string | null;
}) {
  return (
    <div className="flex flex-wrap gap-2">
      {attachments.map((attachment, index) => {
        const filename = attachment.filename?.trim();
        const name = filename === undefined || filename === '' ? attachment.mime_type : filename;
        return attachment.kind === 'image' ? (
          <AttachmentImage
            key={`${attachment.blob_id}-${String(index)}`}
            blobId={attachment.blob_id}
            alt={name}
            baseUrl={baseUrl}
            adminToken={adminToken}
          />
        ) : (
          <button
            key={`${attachment.blob_id}-${String(index)}`}
            type="button"
            title={`${name} · ${attachment.mime_type} — click to download`}
            onClick={() => {
              void downloadBlob(baseUrl, attachment.blob_id, adminToken, name);
            }}
            className="flex max-w-full items-center gap-1.5 rounded-md border-2 border-black bg-canvas px-2 py-1 font-mono text-[0.7rem] hover:bg-brand"
          >
            <RiFileLine className="shrink-0 text-sm" />
            <span className="truncate">{name}</span>
            <RiDownloadLine className="shrink-0 text-sm text-ink-soft" />
          </button>
        );
      })}
    </div>
  );
}
