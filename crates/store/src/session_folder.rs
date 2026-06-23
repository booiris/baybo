//! Persistence interface for user-created chat-session folders.
//!
//! Folders form a two-level tree (`parent_id`); sessions point into a folder
//! via the flat `sessions.folder_id` column (see [`crate::SessionStore`]).
//! The folder table is the **parent** entity — it never owns session rows —
//! so [`SessionFolderStore::delete`] is an organisational dissolve, never a
//! cascade that removes conversations.

use async_trait::async_trait;
use baybo_model::FolderId;
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of `session_folders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFolderRow {
    pub id: FolderId,
    /// `None` = top-level; a non-`None` parent is itself always top-level
    /// (the depth-2 cap is enforced by the session manager, not the store).
    pub parent_id: Option<FolderId>,
    pub name: String,
    /// Manual order within the sibling group (same `parent_id`), ascending.
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

/// Folder lifecycle persistence. The store is a dumb writer — depth and
/// cycle validation live one layer up (the session manager) because they
/// need to read the would-be parent's own `parent_id`.
#[async_trait]
pub trait SessionFolderStore: Send + Sync {
    /// Every folder, ordered by `parent_id` then `position` so callers can
    /// rebuild the sibling-ordered tree without a second sort.
    async fn list(&self) -> Result<Vec<SessionFolderRow>>;

    /// Fetch a single folder, or `None` if it doesn't exist.
    async fn get(&self, id: &FolderId) -> Result<Option<SessionFolderRow>>;

    /// Insert a new folder row verbatim.
    async fn create(&self, row: &SessionFolderRow) -> Result<()>;

    /// Rename a folder. Returns `Ok(false)` if no row matched.
    async fn rename(&self, id: &FolderId, name: &str) -> Result<bool>;

    /// Move a folder under a new parent (`None` = promote to top-level) and
    /// place it at `position`. Returns `Ok(false)` if no row matched.
    async fn reparent(
        &self,
        id: &FolderId,
        parent_id: Option<&FolderId>,
        position: i64,
    ) -> Result<bool>;

    /// Renumber a sibling group: every id in `ordered_ids` (all sharing
    /// `parent_id`) gets its `position` set to its index in the slice. Runs
    /// in one transaction so a partial reorder never lands.
    async fn reorder(&self, parent_id: Option<&FolderId>, ordered_ids: &[FolderId]) -> Result<()>;

    /// Organisational delete: in one transaction, null the `folder_id` of
    /// every session filed directly under `id` (back to uncategorized),
    /// promote every direct sub-folder to top-level (`parent_id = NULL`),
    /// then remove the folder row. **Never** deletes session rows. Returns
    /// `Ok(false)` if no folder matched.
    ///
    /// Returns the ids of sessions whose assignment was cleared so the
    /// caller can broadcast per-session "now uncategorized" patches for live
    /// convergence without a refetch.
    async fn delete(&self, id: &FolderId) -> Result<Option<Vec<baybo_model::SessionId>>>;
}
