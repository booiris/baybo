use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Image {
        blob: BlobRef,
        mime_type: String,
    },
    Audio {
        blob: BlobRef,
        mime_type: String,
    },
    File {
        blob: BlobRef,
        filename: String,
        mime_type: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    pub blob_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {}
