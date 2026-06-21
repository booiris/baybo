use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Upper bound on a folder's display name length, enforced at the gateway
/// before a create/rename reaches the store. Single source of truth for
/// every validation site.
pub const MAX_FOLDER_NAME_LEN: usize = 60;

/// Server-generated identifier for a chat-session folder.
///
/// Opaque string (a ULID at genesis); the store and gateway treat it as a
/// key and never inspect internal structure. Mirrors [`crate::SessionId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FolderId(String);

impl FolderId {
    /// Mint a fresh folder id (a ULID rendered as its canonical string).
    pub fn generate() -> Self {
        Self(Ulid::new().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for FolderId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for FolderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<FolderId> for String {
    fn from(value: FolderId) -> Self {
        value.0
    }
}

/// A user-created folder for organising the chat-session list.
///
/// Folders form a two-level tree via `parent_id` (`None` = top-level; a
/// non-`None` parent is always itself top-level — the depth cap of 2 is
/// enforced in the session manager). Sessions point *into* a folder via the
/// flat `sessions.folder_id` column; a folder never owns session rows, so
/// deleting one only dissolves the grouping (see the storage layer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub id: FolderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<FolderId>,
    pub name: String,
    /// Manual order within the sibling group (same `parent_id`), ascending.
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_id_round_trips_as_string() {
        let id = FolderId::from("fld-abc");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"fld-abc\"");
        let back: FolderId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn generated_folder_ids_are_unique() {
        assert_ne!(FolderId::generate(), FolderId::generate());
    }

    #[test]
    fn folder_summary_round_trips() {
        let f = FolderSummary {
            id: FolderId::from("f1"),
            parent_id: Some(FolderId::from("p1")),
            name: "Work".to_owned(),
            position: 3,
            created_at: Utc::now(),
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: FolderSummary = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn folder_summary_omits_absent_parent() {
        let f = FolderSummary {
            id: FolderId::from("f1"),
            parent_id: None,
            name: "Top".to_owned(),
            position: 0,
            created_at: Utc::now(),
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(
            !s.contains("parent_id"),
            "absent parent must not serialize: {s}"
        );
    }
}
