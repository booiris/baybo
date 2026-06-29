// Typed wrappers around the Tauri blob-leg commands (`blob_download` /
// `blob_upload`). Each runs over its own dedicated relay leg (the Rust shell dials
// `/content/join` with the `x-relay-leg-class: blob` header), so a bulk transfer
// never stalls the chat leg. The crypto + chunking live in `baybo-mobile-core`;
// these are the JS entry points the chat view calls to fetch an attachment for
// display or stage a file the user picked before sending it.

import { Channel, invoke } from "@tauri-apps/api/core";

/** A media reference carried on a chat message — mirrors the Rust
 * `wire::WireAttachment` (snake_case fields, as sent over `chat_send` and
 * received on inbound frames). The bytes never ride the message; only this id
 * does — fetch them with {@link imageObjectUrl}. */
export type WireAttachment = {
  kind: "image" | "audio" | "file";
  blob_id: string;
  mime_type: string;
  size: number;
  filename?: string;
};

/** Bucket a mime type into the wire attachment kind — same rule the web chat and
 * gateway use (the kind selects the agent-side content block). */
export function attachmentKind(mime: string): WireAttachment["kind"] {
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  return "file";
}

/** Which chat transport a blob op rides: the relay's E2E blob leg, or the direct
 * (web-identity) `/v1/blobs` HTTP endpoint. */
export type ChatTransport = "relay" | "direct";

/** Cumulative bytes transferred so far (for a progress bar). */
export type BlobProgress = (bytesSoFar: number) => void;

/**
 * Download the attachment `blobId` to `destPath` on the device, resuming from a
 * partial file if one is already there. `onProgress` is called with the running
 * byte count. Resolves once the bytes are on disk and the content digest is
 * verified against the id; rejects with the gateway's reason otherwise.
 */
export async function downloadBlob(
  blobId: string,
  destPath: string,
  onProgress?: BlobProgress,
): Promise<void> {
  const progress = new Channel<number>();
  if (onProgress) {
    progress.onmessage = onProgress;
  }
  await invoke("blob_download", { blobId, destPath, onProgress: progress });
}

/**
 * Upload the local file at `srcPath` as `mimeType` and resolve to the
 * content-addressed `blob_id` to reference in the next message. `onProgress` is
 * called with the running byte count.
 */
export async function uploadBlob(
  srcPath: string,
  mimeType: string,
  onProgress?: BlobProgress,
): Promise<string> {
  const progress = new Channel<number>();
  if (onProgress) {
    progress.onmessage = onProgress;
  }
  return invoke<string>("blob_upload", { srcPath, mimeType, onProgress: progress });
}

/**
 * Upload raw image `bytes` the webview read from a user-picked `File` (iOS hands
 * the webview a `File`, not a path), returning the content-addressed `blob_id` to
 * reference in the next message. The bytes ride the IPC bridge as a raw body
 * (efficient — not a JSON number array); the mime travels in a header. No progress
 * channel on this path — picked images upload near-instantly.
 */
export async function uploadBytes(
  bytes: ArrayBuffer,
  mimeType: string,
  transport: ChatTransport = "relay",
): Promise<string> {
  // The raw bytes occupy the JSON arg slot, so the mime and the chat leg both ride
  // headers (the backend reads `x-baybo-leg` to route relay vs direct).
  return invoke<string>("blob_upload_bytes", bytes, {
    headers: {
      "x-baybo-mime": mimeType || "application/octet-stream",
      "x-baybo-leg": transport,
    },
  });
}

/**
 * Fetch attachment `blobId` over a blob leg (cached on device by content hash, and
 * digest-verified during download) and return an object URL to use as an `<img>`
 * src. The caller owns the URL and must `URL.revokeObjectURL` it when done.
 */
export async function imageObjectUrl(
  blobId: string,
  mimeType: string,
  transport: ChatTransport = "relay",
): Promise<string> {
  const buf = await invoke<ArrayBuffer>("blob_image", { leg: transport, blobId });
  return URL.createObjectURL(new Blob([buf], { type: mimeType || "application/octet-stream" }));
}
