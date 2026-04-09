use aura_core::ContentBlock;
use serde_json::Value;

/// Convert a list of ContentBlocks to the OpenAI messages content array format.
/// OpenAI expects: [{"type": "text", "text": "..."}, {"type": "image_url", "image_url": {"url": "..."}}]
pub fn to_openai_content(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => serde_json::json!({
                "type": "text",
                "text": text,
            }),
            ContentBlock::Image { blob, mime_type } => serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};blob_id={}", mime_type, blob.blob_id),
                }
            }),
            ContentBlock::Audio { blob, mime_type } => serde_json::json!({
                "type": "text",
                "text": format!("[audio: {} blob_id={}]", mime_type, blob.blob_id),
            }),
            ContentBlock::File {
                blob,
                filename,
                mime_type,
            } => serde_json::json!({
                "type": "text",
                "text": format!("[file: {} ({}) blob_id={}]", filename, mime_type, blob.blob_id),
            }),
        })
        .collect()
}

/// Convert a list of ContentBlocks to the Anthropic messages content array format.
/// Anthropic expects: [{"type": "text", "text": "..."}, {"type": "image", "source": {"type": "base64", ...}}]
pub fn to_anthropic_content(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => serde_json::json!({
                "type": "text",
                "text": text,
            }),
            ContentBlock::Image { blob, mime_type } => serde_json::json!({
                "type": "image",
                "source": {
                    "type": "blob_ref",
                    "blob_id": blob.blob_id,
                    "media_type": mime_type,
                }
            }),
            ContentBlock::Audio { blob, mime_type } => serde_json::json!({
                "type": "text",
                "text": format!("[audio: {} blob_id={}]", mime_type, blob.blob_id),
            }),
            ContentBlock::File {
                blob,
                filename,
                mime_type,
            } => serde_json::json!({
                "type": "text",
                "text": format!("[file: {} ({}) blob_id={}]", filename, mime_type, blob.blob_id),
            }),
        })
        .collect()
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
    use aura_core::BlobRef;

    fn sample_blob() -> BlobRef {
        BlobRef {
            blob_id: "blob_123".to_string(),
        }
    }

    #[test]
    fn openai_text_only() {
        let blocks = vec![ContentBlock::Text("hello".to_string())];
        let result = to_openai_content(&blocks);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "text");
        assert_eq!(result[0]["text"], "hello");
    }

    #[test]
    fn openai_with_image() {
        let blocks = vec![
            ContentBlock::Text("look at this".to_string()),
            ContentBlock::Image {
                blob: sample_blob(),
                mime_type: "image/png".to_string(),
            },
        ];
        let result = to_openai_content(&blocks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1]["type"], "image_url");
    }

    #[test]
    fn anthropic_text_only() {
        let blocks = vec![ContentBlock::Text("hello".to_string())];
        let result = to_anthropic_content(&blocks);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "text");
    }

    #[test]
    fn anthropic_with_image() {
        let blocks = vec![ContentBlock::Image {
            blob: sample_blob(),
            mime_type: "image/jpeg".to_string(),
        }];
        let result = to_anthropic_content(&blocks);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "image");
        assert_eq!(result[0]["source"]["media_type"], "image/jpeg");
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

    #[test]
    fn openai_audio_as_text() {
        let blocks = vec![ContentBlock::Audio {
            blob: sample_blob(),
            mime_type: "audio/mp3".to_string(),
        }];
        let result = to_openai_content(&blocks);
        assert_eq!(result[0]["type"], "text");
        assert!(result[0]["text"].as_str().unwrap().contains("audio"));
    }

    #[test]
    fn openai_file_as_text() {
        let blocks = vec![ContentBlock::File {
            blob: sample_blob(),
            filename: "doc.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
        }];
        let result = to_openai_content(&blocks);
        assert_eq!(result[0]["type"], "text");
        assert!(result[0]["text"].as_str().unwrap().contains("doc.pdf"));
    }

    #[test]
    fn anthropic_file_as_text() {
        let blocks = vec![ContentBlock::File {
            blob: sample_blob(),
            filename: "doc.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
        }];
        let result = to_anthropic_content(&blocks);
        assert_eq!(result[0]["type"], "text");
    }
}
