//! Turning a **stored blob** into the [`ContentBlock`] a model is handed,
//! with every price input read off the bytes rather than off whoever
//! uploaded them.
//!
//! This is the blob-store twin of `builtin::attach_file`'s probes, which
//! answer the same questions about a file still on local disk. Both exist
//! because the two producers hold different things — a path and a
//! capability id — and neither can reach the other's source; what they
//! must not do is disagree about what a probe costs or when it is worth
//! taking, which is why the caps and the reasoning live in `baybo-llm` and
//! are only spent here.
//!
//! Two callers, deliberately: the gateway's chat ingest, and the kanban
//! board's run brief. A third copy of "which cap does an image probe get"
//! is exactly the drift this module exists to prevent.

use baybo_model::{BlobRef, MediaBlock, MediaKind};
use baybo_store::BlobStore;

/// Build the block for one stored blob, probing whatever its kind prices on.
///
/// `filename` is the only thing here the caller owns: the blob store has no
/// filename column, so a name is either supplied or genuinely absent.
pub async fn probed_block(
    blob_store: &dyn BlobStore,
    blob_id: String,
    mime_type: String,
    filename: Option<String>,
) -> MediaBlock {
    let blob = BlobRef {
        blob_id: blob_id.clone(),
    };
    match MediaKind::of_mime(&mime_type) {
        MediaKind::Image => {
            let (width, height) = probe_image_dimensions(blob_store, &blob_id).await.unzip();
            MediaBlock::image(blob, mime_type, filename, width, height)
        }
        MediaKind::Audio => {
            let duration_ms = probe_audio_duration_ms(blob_store, &blob_id).await;
            MediaBlock::audio(blob, mime_type, filename, duration_ms)
        }
        MediaKind::File => {
            let page_count = probe_pdf_page_count(blob_store, &blob_id, &mime_type).await;
            let size_bytes = stat_blob_size(blob_store, &blob_id).await;
            MediaBlock::file(
                blob,
                filename.unwrap_or_default(),
                mime_type,
                None,
                page_count,
                size_bytes,
            )
        }
    }
}

/// Byte length of a stored blob, straight off the metadata row. A client's
/// own claim about the same blob is not used: the context budget spends
/// this number.
pub async fn stat_blob_size(blob_store: &dyn BlobStore, blob_id: &str) -> Option<u32> {
    match blob_store.stat(blob_id).await {
        Ok(meta) => u32::try_from(meta.size).ok(),
        Err(e) => {
            tracing::debug!(%blob_id, error = %e, "attachment stat failed; size unknown");
            None
        }
    }
}

/// Read a blob for probing, refusing anything the delivery path would
/// reject on size. `stat` first so an oversize payload is never pulled
/// into memory.
///
/// `max_bytes` is the DELIVERY cap of the arm being probed, not a shared
/// worst case: above it the LLM layer always stubs the block, so a fact
/// recovered from those bytes would be a price charged for something that
/// costs the stub. Reading the wider of the two caps charged an 8-16 MiB
/// PDF its full page price for a block that can never be delivered.
async fn probe_bytes(blob_store: &dyn BlobStore, blob_id: &str, max_bytes: u64) -> Option<Vec<u8>> {
    match blob_store.stat(blob_id).await {
        Ok(meta) if meta.size <= max_bytes => {}
        Ok(meta) => {
            tracing::debug!(%blob_id, size = meta.size, limit = max_bytes, "attachment too large to probe");
            return None;
        }
        Err(e) => {
            tracing::debug!(%blob_id, error = %e, "attachment stat failed; skipping probe");
            return None;
        }
    }
    blob_store.get(blob_id).await.ok()
}

/// Pages in a stored PDF, probed because the `ContentBlock` that outlives
/// the bytes is all the context budget ever sees. A provider bills a native
/// document per PAGE, and byte count is not a stand-in — measured, real
/// documents run 10 to 4,007 bytes per page.
pub async fn probe_pdf_page_count(
    blob_store: &dyn BlobStore,
    blob_id: &str,
    mime_type: &str,
) -> Option<u32> {
    if !baybo_llm::delivers_pdf_document(mime_type) {
        return None;
    }
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_PDF_DOCUMENT_BYTES as u64,
    )
    .await?;
    // `spawn_blocking`: a whole-payload parse is CPU-bound, and a panic
    // inside it surfaces as a `JoinError` instead of unwinding the reactor.
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::pdf_page_count(&bytes))
        .await
        .ok()
        .flatten()
}

pub async fn probe_audio_duration_ms(blob_store: &dyn BlobStore, blob_id: &str) -> Option<u32> {
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_AUDIO_DOCUMENT_BYTES as u64,
    )
    .await?;
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::audio_duration_ms(&bytes))
        .await
        .ok()
        .flatten()
        .filter(|ms| *ms > 0)
}

/// Pixel dimensions of a stored image, probed for the same reason the page
/// count is: a provider bills an image per tile of its pixel grid.
pub async fn probe_image_dimensions(
    blob_store: &dyn BlobStore,
    blob_id: &str,
) -> Option<(u32, u32)> {
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_IMAGE_DOCUMENT_BYTES as u64,
    )
    .await?;
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::image_dimensions(&bytes))
        .await
        .ok()
        .flatten()
}
