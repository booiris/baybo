use aura_model::ContentBlock;

/// Convert a ContentBlock to a textual representation.
/// Text blocks return the text directly; non-text blocks produce a descriptive placeholder.
pub fn content_block_to_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.clone(),
        ContentBlock::Image { blob, mime_type } => {
            format!("[image: {} blob_id={}]", mime_type, blob.blob_id)
        }
        ContentBlock::Audio { blob, mime_type } => {
            format!("[audio: {} blob_id={}]", mime_type, blob.blob_id)
        }
        ContentBlock::File {
            blob,
            filename,
            mime_type,
        } => {
            format!(
                "[file: {} ({}) blob_id={}]",
                filename, mime_type, blob.blob_id
            )
        }
    }
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
    use aura_model::BlobRef;

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
            },
            ContentBlock::Text("second".to_string()),
        ];
        assert_eq!(extract_text(&blocks), "first\nsecond");
    }

    #[test]
    fn extract_text_empty() {
        assert_eq!(extract_text(&[]), "");
    }
}
