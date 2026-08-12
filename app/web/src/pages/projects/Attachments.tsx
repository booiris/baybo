import { useCallback, useRef, useState } from 'react';
import type { ChangeEvent, ClipboardEvent, DragEvent } from 'react';
import { RiAttachment2, RiCloseLine, RiDownloadLine, RiFileLine, RiLoader4Line } from 'react-icons/ri';

import { useAuth } from '../../api/auth';
import { downloadBlob, uploadBlob, useBlobUrl } from '../../api/blobs';
import { AttachmentImage } from '../chat/AttachmentImage';
import { ImageLightbox } from '../chat/ImageLightbox';
import type { components } from '../../api/schema';

export type IssueAttachment = components['schemas']['IssueAttachmentDto'];
export type IssueAttachmentRequest = components['schemas']['IssueAttachmentRequest'];

/// Which files a card may carry, mirroring `baybo_project::MAX_ISSUE_ATTACHMENTS`.
///
/// A second copy of a server rule, deliberately: the server refuses over the
/// limit whatever the client does, and this exists only so the operator is
/// told before a fifteen-megabyte upload rather than after it. If they ever
/// disagree the server wins and the user sees an error — which is the safe
/// direction for a duplicate to fail in.
export const MAX_ISSUE_ATTACHMENTS = 16;

/// Whether a file is one the dashboard can draw rather than only name.
///
/// Derived from the mime on this side of the wire; the server does the same
/// derivation on its own side for the model's content blocks. Two readings
/// of one rule, one per side, neither with a second copy — the `accept`
/// grammar's arrangement in deck.
export function isImage(mimeType: string): boolean {
  return mimeType.toLowerCase().startsWith('image/');
}

/// What a file is called, for a file that may have arrived without a name.
/// A display fallback only — never written back as the file's name.
export function attachmentName(a: IssueAttachment): string {
  const named = a.filename?.trim();
  return named !== undefined && named.length > 0 ? named : UNNAMED;
}

/// The same fallback for a draft entry.
function draftName(a: DraftAttachment): string {
  const named = a.filename?.trim();
  return named !== undefined && named.length > 0 ? named : UNNAMED;
}

const UNNAMED = 'attachment';

/// One file in a draft: uploading, uploaded, or failed.
///
/// `localId` rather than `blobId` as the identity, because a file has no
/// blob id until its upload lands and the operator can remove it before
/// then — and two picks of the same bytes are two rows the user can delete
/// independently, which a content-addressed id could not tell apart.
export type DraftAttachment = {
  localId: string;
  /// The name the file was uploaded under, or absent for one that genuinely
  /// has none. **Not** defaulted to a placeholder here: a draft is what gets
  /// saved back, so a stand-in name would be persisted as the file's real
  /// one the next time anything on the card is written.
  filename?: string;
  mimeType: string;
  size: number;
  status: 'uploading' | 'ready' | 'error';
  blobId?: string;
  /// Object URL of the **local** file, for an image the operator just picked
  /// — so the thumbnail is on screen before the upload is, which is what
  /// chat's composer does. Absent for a file adopted from the card, which
  /// has no local `File` to point at and is fetched by blob id instead.
  previewUrl?: string;
};

/// Give back every object URL a list is holding. Missing this leaks the
/// bytes of every image ever picked in the session — the URL keeps the
/// `Blob` alive on its own.
function revokePreviews(attachments: DraftAttachment[]) {
  for (const a of attachments) {
    if (a.previewUrl !== undefined) URL.revokeObjectURL(a.previewUrl);
  }
}

let nextLocalId = 0;

/// The attachment half of a composer: the files, how they get uploaded, and
/// the handlers a text box needs to accept a paste or a drop.
///
/// One hook for all three of this board's prose widgets — the create modal's
/// textarea, the description's Milkdown editor and the timeline's comment
/// box — because they already differ in every other way, and "what happens
/// when you drop a PNG on it" is the one thing they must not differ in.
export function useAttachmentDraft(projectId: string) {
  const { baseUrl, token } = useAuth();
  const [attachments, setAttachments] = useState<DraftAttachment[]>([]);
  /// Which card's stored files this draft was seeded from, or `null` for a
  /// composer that starts empty.
  ///
  /// **State, not a ref**, and that is the whole point: a caller that saves
  /// whenever the draft and the card disagree must not run before the seed
  /// it asked for is visible. A ref is set during the seeding effect and
  /// read by the *next* effect in the same commit — where the adopted files
  /// have not landed yet, so the draft still reads empty and the save wipes
  /// the card. Both `setState`s below are batched into one commit, so
  /// "seeded" and "the files" become true together or not at all.
  const [seededFor, setSeededFor] = useState<string | null>(null);
  const countRef = useRef(0);
  countRef.current = attachments.length;

  const add = useCallback(
    (files: File[]) => {
      const room = MAX_ISSUE_ATTACHMENTS - countRef.current;
      for (const file of files.slice(0, Math.max(room, 0))) {
        const localId = `a${String(nextLocalId++)}`;
        const mimeType = file.type || 'application/octet-stream';
        const previewUrl = isImage(mimeType) ? URL.createObjectURL(file) : undefined;
        setAttachments((prev) => [
          ...prev,
          {
            localId,
            filename: file.name,
            mimeType,
            size: file.size,
            status: 'uploading',
            previewUrl,
          },
        ]);
        void uploadBlob(baseUrl, token, file, { projectId }).then(
          ({ blobId }) => {
            setAttachments((prev) =>
              prev.map((a) => (a.localId === localId ? { ...a, status: 'ready', blobId } : a)),
            );
          },
          () => {
            setAttachments((prev) =>
              prev.map((a) => (a.localId === localId ? { ...a, status: 'error' } : a)),
            );
          },
        );
      }
    },
    [baseUrl, token, projectId],
  );

  const remove = useCallback((localId: string) => {
    setAttachments((prev) => {
      revokePreviews(prev.filter((a) => a.localId === localId));
      return prev.filter((a) => a.localId !== localId);
    });
  }, []);

  const clear = useCallback(() => {
    setAttachments((prev) => {
      revokePreviews(prev);
      return [];
    });
  }, []);

  /// Adopt a card's stored files so an edit starts from what is on it.
  /// `card` names what was adopted, and comes back out as `seededFor`.
  const adopt = useCallback((card: string, stored: IssueAttachment[]) => {
    setAttachments((prev) => {
      revokePreviews(prev);
      return stored.map((a) => ({
        localId: `a${String(nextLocalId++)}`,
        filename: a.filename ?? undefined,
        mimeType: a.mime_type,
        size: a.size,
        status: 'ready' as const,
        blobId: a.blob_id,
      }));
    });
    setSeededFor(card);
  }, []);

  /// A paste that carries files takes them; one that carries only text is
  /// left alone, or pasting a paragraph into the description would be
  /// intercepted by an attachment handler.
  const onPaste = useCallback(
    (event: ClipboardEvent) => {
      const files = Array.from(event.clipboardData.files);
      if (files.length === 0) return;
      event.preventDefault();
      add(files);
    },
    [add],
  );

  const onDrop = useCallback(
    (event: DragEvent) => {
      const files = Array.from(event.dataTransfer.files);
      if (files.length === 0) return;
      event.preventDefault();
      add(files);
    },
    [add],
  );

  /// Without this the browser navigates away to the dropped file, which
  /// loses the draft — a drop handler alone is not enough.
  const onDragOver = useCallback((event: DragEvent) => {
    if (event.dataTransfer.types.includes('Files')) event.preventDefault();
  }, []);

  return { attachments, seededFor, add, remove, clear, adopt, onPaste, onDrop, onDragOver };
}

/// What a draft sends: the ids that landed, and nothing about the files that
/// did not. An upload still in flight is simply not part of the request —
/// the composer disables submit while any is pending, so this is the
/// belt to that braces.
export function readyRequests(attachments: DraftAttachment[]): IssueAttachmentRequest[] {
  return attachments
    .filter((a) => a.status === 'ready' && a.blobId !== undefined)
    .map((a) => ({ blob_id: a.blobId ?? '', ...(a.filename == null ? {} : { filename: a.filename }) }));
}

export function anyUploading(attachments: DraftAttachment[]): boolean {
  return attachments.some((a) => a.status === 'uploading');
}

const CHIP =
  'flex items-center gap-1.5 max-w-[220px] px-2 py-1 bg-canvas border-2 border-black rounded-md font-mono text-[0.7rem]';

function humanSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024).toString()} KB`;
  return `${String(bytes)} B`;
}

/// The attach button. Its own component because all three composers need it
/// and each puts it somewhere different.
export function AttachButton({
  onPick,
  disabled,
  full,
  subtle,
}: {
  onPick: (files: File[]) => void;
  disabled?: boolean;
  /// The draft is at the limit: the control stays visible and says why,
  /// rather than disappearing and leaving the operator looking for it.
  full?: boolean;
  /// Drop the border and the raised surface.
  ///
  /// The bordered face belongs to a control sitting **inside** composer
  /// chrome — a modal footer, a composer pill — where it reads as one of a
  /// row of buttons. The description has no such chrome: its button sits
  /// bare on the page under the prose, where 2px of ink makes an optional
  /// affordance the loudest thing in the pane.
  subtle?: boolean;
}) {
  const input = useRef<HTMLInputElement | null>(null);
  return (
    <>
      <input
        ref={input}
        type="file"
        multiple
        className="hidden"
        onChange={(event: ChangeEvent<HTMLInputElement>) => {
          const picked = event.target.files;
          if (picked !== null) onPick(Array.from(picked));
          // Reset so picking the same file again still fires `change`.
          event.target.value = '';
        }}
      />
      <button
        type="button"
        aria-label="Attach files"
        title={full === true ? `At most ${String(MAX_ISSUE_ATTACHMENTS)} files` : 'Attach files'}
        disabled={disabled === true || full === true}
        onClick={() => input.current?.click()}
        className={`flex h-7 w-7 items-center justify-center rounded-md disabled:opacity-40 disabled:cursor-not-allowed ${
          subtle === true
            ? 'text-ink-soft hover:bg-canvas hover:text-ink'
            : 'border-2 border-black bg-surface text-ink hover:bg-brand'
        }`}
      >
        <RiAttachment2 className="text-sm" />
      </button>
    </>
  );
}

/// One file in a draft: chat's composer shape, so the two boxes that take an
/// attachment look the same.
///
/// An image is a **square thumbnail with the ✕ on its corner** rather than a
/// chip with its name beside it — a picture identifies itself, and the name
/// was spending a row's width to repeat what the pixels already said.
/// Everything the page cannot draw keeps the chip, because a filename is all
/// such a file has.
///
/// Its own component because it takes a hook: a stored image has to be
/// fetched with the operator's bearer, and hooks cannot be called from inside
/// a `map`.
function TrayChip({
  attachment: a,
  onRemove,
}: {
  attachment: DraftAttachment;
  onRemove: (localId: string) => void;
}) {
  const { baseUrl, token } = useAuth();
  const [viewing, setViewing] = useState(false);
  const name = draftName(a);
  const drawable = isImage(a.mimeType);
  // A freshly picked file already has a local URL; one adopted from the card
  // has only a blob id and must be fetched. `useBlobUrl` no-ops on null.
  const fetched = useBlobUrl(
    drawable && a.previewUrl === undefined ? a.blobId : null,
    baseUrl,
    token,
  );
  const src = a.previewUrl ?? fetched;

  const remove = (
    <button
      type="button"
      aria-label={`Remove ${name}`}
      onClick={() => {
        onRemove(a.localId);
      }}
      className={
        drawable
          ? 'absolute -top-1.5 -right-1.5 flex h-4 w-4 items-center justify-center rounded-full border-2 border-black bg-white text-ink hover:bg-err hover:text-white'
          : 'shrink-0 text-ink-soft hover:text-err'
      }
    >
      <RiCloseLine className={drawable ? 'text-[0.6rem]' : 'text-sm'} />
    </button>
  );

  if (drawable) {
    return (
      <span className="relative inline-flex shrink-0">
        {src == null ? (
          <span className="flex h-14 w-14 items-center justify-center rounded-md border-2 border-black bg-canvas">
            <RiLoader4Line className="animate-spin text-base" />
          </span>
        ) : (
          // Clickable, unlike chat's composer preview: these are a card's
          // stored files as often as a fresh pick, and a 56px square is not
          // where you check whether the mockup is the right one.
          <button
            type="button"
            aria-label={`View ${name}`}
            title={`${name} · ${humanSize(a.size)}`}
            onClick={() => {
              setViewing(true);
            }}
            className="inline-flex"
          >
            <img
              src={src}
              alt={name}
              className={`h-14 w-14 rounded-md border-2 border-black object-cover ${
                a.status === 'error' ? 'opacity-40' : ''
              }`}
            />
          </button>
        )}
        {a.status === 'uploading' && src != null ? (
          <span className="absolute inset-0 flex items-center justify-center rounded-md bg-white/60">
            <RiLoader4Line className="animate-spin text-base" />
          </span>
        ) : null}
        {remove}
        {viewing && src != null ? (
          <ImageLightbox
            url={src}
            alt={name}
            onClose={() => {
              setViewing(false);
            }}
          />
        ) : null}
      </span>
    );
  }

  return (
    <span
      className={`${CHIP} ${a.status === 'error' ? 'border-err text-err' : ''}`}
      title={`${name} · ${humanSize(a.size)}`}
    >
      {a.status === 'uploading' ? (
        <RiLoader4Line className="text-sm shrink-0 animate-spin" />
      ) : (
        <RiFileLine className="text-sm shrink-0" />
      )}
      {/* The name is the download, for the same reason the timeline's
          read-only chip is one: a file the page cannot draw has no other way
          out of the card. A sibling of the ✕ rather than wrapping it — a
          button inside a button is not a thing. */}
      {a.status === 'ready' && a.blobId !== undefined ? (
        <button
          type="button"
          title={`${name} · ${a.mimeType} · ${humanSize(a.size)} — click to download`}
          onClick={() => {
            void downloadBlob(baseUrl, a.blobId ?? '', token, name);
          }}
          className="flex min-w-0 items-center gap-1 truncate hover:underline"
        >
          <span className="truncate">{name}</span>
          <RiDownloadLine className="shrink-0 text-ink-soft" />
        </button>
      ) : (
        <span className="truncate">{name}</span>
      )}
      {remove}
    </span>
  );
}

/// A draft's files, each removable.
export function AttachmentTray({
  attachments,
  onRemove,
}: {
  attachments: DraftAttachment[];
  onRemove: (localId: string) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="flex flex-wrap items-start gap-2">
      {attachments.map((a) => (
        <TrayChip key={a.localId} attachment={a} onRemove={onRemove} />
      ))}
    </div>
  );
}

/// Stored files, as a reader sees them: images drawn, everything else a chip
/// that downloads.
export function AttachmentList({ attachments }: { attachments: IssueAttachment[] }) {
  const { baseUrl, token } = useAuth();
  if (attachments.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2">
      {attachments.map((a) => {
        const name = attachmentName(a);
        return isImage(a.mime_type) ? (
          <AttachmentImage
            key={a.blob_id}
            blobId={a.blob_id}
            alt={name}
            baseUrl={baseUrl}
            adminToken={token}
          />
        ) : (
          <button
            key={a.blob_id}
            type="button"
            title={`${name} · ${a.mime_type} · ${humanSize(a.size)} — click to download`}
            onClick={() => {
              void downloadBlob(baseUrl, a.blob_id, token, name);
            }}
            className={`${CHIP} hover:bg-brand`}
          >
            <RiFileLine className="text-sm shrink-0" />
            <span className="truncate">{name}</span>
            <RiDownloadLine className="text-sm shrink-0 text-ink-soft" />
          </button>
        );
      })}
    </div>
  );
}
