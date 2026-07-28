use baybo_model::ContentBlock;

use crate::render_slots;

/// Attribute and stub slots are truncated to this many characters. A
/// filename arrives from the client at whatever length it likes, and both
/// the things it lands in — the inlined-document wrapper's head line and
/// the `[file: …]` stub — are only useful while they stay readable.
///
/// Shared on purpose. The stub is the *fallback* for the wrapper (a
/// text-like document whose blob fetch fails takes it), so a stub laxer
/// than the wrapper would let the exact bytes the wrapper refuses in
/// through the back door: measured, a 133,334-byte filename rendered a
/// 133,440-byte stub worth 56,447 real tokens where the budget charged
/// 54.
pub(crate) const MAX_SLOT_CHARS: usize = 120;

/// Stands in for whatever [`MAX_SLOT_CHARS`] cut off.
const SLOT_ELISION: char = '…';

const MAX_UTF8_CHAR_BYTES: usize = 4;

/// Widest one sanitized slot can get: [`MAX_SLOT_CHARS`] characters of up
/// to four UTF-8 bytes each, plus the elision mark.
pub(crate) const MAX_SLOT_BYTES: usize =
    MAX_SLOT_CHARS * MAX_UTF8_CHAR_BYTES + SLOT_ELISION.len_utf8();

const IMAGE_STUB_TEMPLATE: &str = "[image: {{mime_type}} blob_id={{blob_id}}]";
const AUDIO_STUB_TEMPLATE: &str = "[audio: {{mime_type}} blob_id={{blob_id}}]";
const FILE_STUB_TEMPLATE: &str = "[file: {{filename}} ({{mime_type}}) blob_id={{blob_id}}]";

/// Upper bound on the bytes a media stub can render — the widest of the
/// three templates with every slot at [`MAX_SLOT_BYTES`]. The `{{…}}`
/// placeholders are counted alongside the values that replace them, which
/// is slack, not error.
///
/// `baybo-context`'s tokenizer charges one token per byte of this, and
/// **no media arm may price below it**: every arm degrades to a stub when
/// the blob fetch fails or the payload is over cap, so an estimate under
/// this number would not cover its own fallback.
pub(crate) const MAX_CONTENT_STUB_BYTES: usize = FILE_STUB_TEMPLATE.len() + 3 * MAX_SLOT_BYTES;

/// Bound and de-fang one slot: a control character would break the
/// wrapper across lines and `"` / `<` / `>` would close it early.
pub(crate) fn sanitize_slot(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (seen, ch) in value.chars().enumerate() {
        if seen == MAX_SLOT_CHARS {
            out.push(SLOT_ELISION);
            break;
        }
        out.push(match ch {
            '"' | '<' | '>' => '_',
            c if c.is_control() => ' ',
            c => c,
        });
    }
    out
}

/// Convert a ContentBlock to a textual representation.
/// Text blocks return the text directly; non-text blocks produce a descriptive placeholder.
pub fn content_block_to_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.clone(),
        ContentBlock::Image {
            blob, mime_type, ..
        } => media_stub(IMAGE_STUB_TEMPLATE, None, mime_type, &blob.blob_id),
        ContentBlock::Audio {
            blob, mime_type, ..
        } => media_stub(AUDIO_STUB_TEMPLATE, None, mime_type, &blob.blob_id),
        ContentBlock::File {
            blob,
            filename,
            mime_type,
            ..
        } => media_stub(FILE_STUB_TEMPLATE, Some(filename), mime_type, &blob.blob_id),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            format!("[tool_use: {name} id={id} input={input}]")
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            format!("[tool_result: id={tool_use_id} content={content}]")
        }
        ContentBlock::Thinking { .. } => "[thinking]".to_string(),
    }
}

fn media_stub(template: &str, filename: Option<&str>, mime_type: &str, blob_id: &str) -> String {
    render_slots(
        template,
        &[
            ("filename", &sanitize_slot(filename.unwrap_or_default())),
            ("mime_type", &sanitize_slot(mime_type)),
            ("blob_id", &sanitize_slot(blob_id)),
        ],
    )
}

/// Extract all text from a list of ContentBlocks, joining with newlines.
pub fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::BlobRef;

    fn sample_blob() -> BlobRef {
        BlobRef {
            blob_id: "blob_123".to_string(),
        }
    }

    #[test]
    fn content_block_to_text_text() {
        let block = ContentBlock::Text("hello".to_string());
        assert_eq!(content_block_to_text(&block), "hello");
    }

    #[test]
    fn content_block_to_text_image() {
        let block = ContentBlock::Image {
            blob: sample_blob(),
            mime_type: "image/png".to_string(),
            filename: None,
            width: None,
            height: None,
        };
        let result = content_block_to_text(&block);
        assert!(result.contains("image/png"));
        assert!(result.contains("blob_123"));
    }

    #[test]
    fn content_block_to_text_audio() {
        let block = ContentBlock::Audio {
            blob: sample_blob(),
            mime_type: "audio/mp3".to_string(),
            filename: None,
            duration_ms: None,
        };
        let result = content_block_to_text(&block);
        assert!(result.contains("audio"));
        assert!(result.contains("blob_123"));
    }

    #[test]
    fn content_block_to_text_file() {
        let block = ContentBlock::File {
            blob: sample_blob(),
            filename: "doc.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            duration_ms: None,
            page_count: None,
            size_bytes: None,
        };
        let result = content_block_to_text(&block);
        assert!(result.contains("doc.pdf"));
        assert!(result.contains("blob_123"));
    }

    #[test]
    fn extract_text_mixed() {
        let blocks = vec![
            ContentBlock::Text("first".to_string()),
            ContentBlock::Image {
                blob: sample_blob(),
                mime_type: "image/png".to_string(),
                filename: None,
                width: None,
                height: None,
            },
            ContentBlock::Text("second".to_string()),
        ];
        assert_eq!(extract_text(&blocks), "first\nsecond");
    }

    #[test]
    fn extract_text_empty() {
        assert_eq!(extract_text(&[]), "");
    }

    /// Pin: nothing caps `filename`, `mime_type` or `blob_id` upstream —
    /// `MAX_MESSAGE_BATCH_TEXT_BYTES` caps `content` and
    /// `MAX_MESSAGE_BATCH_ATTACHMENTS` caps the count, neither of these.
    /// Measured before the bound: a 133,334-byte filename produced a
    /// 133,440-byte stub, 56,447 real tokens against a charge of 54.
    #[test]
    fn a_stub_is_bounded_however_wide_its_inputs_are() {
        let huge = "𝕏".repeat(100_000);
        for block in [
            ContentBlock::File {
                blob: BlobRef {
                    blob_id: huge.clone(),
                },
                filename: huge.clone(),
                mime_type: huge.clone(),
                duration_ms: None,
                page_count: None,
                size_bytes: None,
            },
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: huge.clone(),
                },
                mime_type: huge.clone(),
                filename: Some(huge.clone()),
                width: None,
                height: None,
            },
            ContentBlock::Audio {
                blob: BlobRef {
                    blob_id: huge.clone(),
                },
                mime_type: huge.clone(),
                filename: Some(huge.clone()),
                duration_ms: None,
            },
        ] {
            let stub = content_block_to_text(&block);
            assert!(
                stub.len() <= MAX_CONTENT_STUB_BYTES,
                "{} bytes exceeds the bound {MAX_CONTENT_STUB_BYTES}",
                stub.len()
            );
        }
    }

    /// [`MAX_CONTENT_STUB_BYTES`] is derived from the file template alone,
    /// so the other two must stay inside it.
    #[test]
    fn the_file_template_really_is_the_widest() {
        for template in [IMAGE_STUB_TEMPLATE, AUDIO_STUB_TEMPLATE] {
            assert!(
                template.len() + 3 * MAX_SLOT_BYTES <= MAX_CONTENT_STUB_BYTES,
                "{template}"
            );
        }
    }

    #[test]
    fn an_ordinary_stub_keeps_its_readable_shape() {
        let block = ContentBlock::File {
            blob: BlobRef {
                blob_id: "sha256:abc.tok".into(),
            },
            filename: "notes.md".into(),
            mime_type: "text/markdown".into(),
            duration_ms: None,
            page_count: None,
            size_bytes: None,
        };
        assert_eq!(
            content_block_to_text(&block),
            "[file: notes.md (text/markdown) blob_id=sha256:abc.tok]"
        );
    }
}
