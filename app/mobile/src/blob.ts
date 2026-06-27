// Typed wrappers around the Tauri blob-leg commands (`blob_download` /
// `blob_upload`). Each runs over its own dedicated relay leg (the Rust shell dials
// `/content/join` with the `x-relay-leg-class: blob` header), so a bulk transfer
// never stalls the chat leg. The crypto + chunking live in `baybo-mobile-core`;
// these are the JS entry points the chat view calls to fetch an attachment for
// display or stage a file the user picked before sending it.

import { Channel, invoke } from "@tauri-apps/api/core";

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
