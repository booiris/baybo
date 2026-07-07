//! libsql implementation of [`SessionFolderStore`].

use async_trait::async_trait;
use baybo_model::{FolderId, SessionId};

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::session_folder::{Result, SessionFolderRow, SessionFolderStore};

pub struct LibsqlSessionFolderStore {
    pool: LibsqlPool,
}

impl LibsqlSessionFolderStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn row_from_libsql(row: &libsql::Row) -> Result<SessionFolderRow> {
    let id: String = row
        .get(0)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_folders.id: {e}")))?;
    let parent_id: Option<String> = row
        .get(1)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_folders.parent_id: {e}")))?;
    let name: String = row
        .get(2)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_folders.name: {e}")))?;
    let position: i64 = row
        .get(3)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_folders.position: {e}")))?;
    let created_at_us: i64 = row
        .get(4)
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("session_folders.created_at: {e}")))?;
    let created_at = super::time::from_us(created_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "session_folders.created_at out of range: {created_at_us}"
        ))
    })?;
    Ok(SessionFolderRow {
        id: FolderId::from(id),
        parent_id: parent_id.map(FolderId::from),
        name,
        position,
        created_at,
    })
}

#[async_trait]
impl SessionFolderStore for LibsqlSessionFolderStore {
    async fn list(&self) -> Result<Vec<SessionFolderRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT id, parent_id, name, position, created_at \
                 FROM session_folders ORDER BY parent_id, position, id",
                (),
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql list folders: {e}")))?;
        let mut folders = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql folder row: {e}")))?
        {
            folders.push(row_from_libsql(&row)?);
        }
        Ok(folders)
    }

    async fn get(&self, id: &FolderId) -> Result<Option<SessionFolderRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT id, parent_id, name, position, created_at \
                 FROM session_folders WHERE id = ?1",
                libsql::params![id.as_str().to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get folder: {e}")))?;
        match rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql folder row: {e}")))?
        {
            Some(row) => Ok(Some(row_from_libsql(&row)?)),
            None => Ok(None),
        }
    }

    async fn create(&self, row: &SessionFolderRow) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO session_folders (id, parent_id, name, position, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            libsql::params![
                row.id.as_str().to_string(),
                row.parent_id.as_ref().map(|p| p.as_str().to_string()),
                row.name.clone(),
                row.position,
                super::time::to_us(row.created_at),
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql create folder: {e}")))?;
        Ok(())
    }

    async fn rename(&self, id: &FolderId, name: &str) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE session_folders SET name = ?2 WHERE id = ?1",
                libsql::params![id.as_str().to_string(), name.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql rename folder: {e}")))?;
        Ok(affected > 0)
    }

    async fn reparent(
        &self,
        id: &FolderId,
        parent_id: Option<&FolderId>,
        position: i64,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE session_folders SET parent_id = ?2, position = ?3 WHERE id = ?1",
                libsql::params![
                    id.as_str().to_string(),
                    parent_id.map(|p| p.as_str().to_string()),
                    position,
                ],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql reparent folder: {e}")))?;
        Ok(affected > 0)
    }

    async fn reorder(&self, parent_id: Option<&FolderId>, ordered_ids: &[FolderId]) -> Result<()> {
        let conn = self.pool.conn();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql begin reorder tx: {e}")))?;
        let parent = parent_id.map(|p| p.as_str().to_string());
        for (idx, id) in ordered_ids.iter().enumerate() {
            // `parent_id IS ?3` is NULL-safe: a NULL bind matches top-level
            // rows, a value bind matches that exact parent — so a stray id
            // from a different group can't be renumbered into this one.
            tx.execute(
                "UPDATE session_folders SET position = ?2 \
                 WHERE id = ?1 AND parent_id IS ?3",
                libsql::params![id.as_str().to_string(), idx as i64, parent.clone()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql reorder folder: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql commit reorder: {e}")))?;
        Ok(())
    }

    async fn delete(&self, id: &FolderId) -> Result<Option<Vec<SessionId>>> {
        let conn = self.pool.conn();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql begin delete-folder tx: {e}"))
            })?;
        let id_str = id.as_str().to_string();

        // Sessions filed directly under this folder, collected before they
        // are nulled so the caller can broadcast their "now uncategorized"
        // patches for live convergence.
        let mut affected_sessions = Vec::new();
        {
            let mut rows = tx
                .query(
                    "SELECT id FROM sessions WHERE folder_id = ?1",
                    libsql::params![id_str.clone()],
                )
                .await
                .map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql folder member query: {e}"))
                })?;
            while let Some(row) = rows.next().await.map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql folder member row: {e}"))
            })? {
                let sid: String = row.get(0).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql folder member id: {e}"))
                })?;
                affected_sessions.push(SessionId::from(sid));
            }
        }

        // Promote sub-folders to top-level, shifted above the current
        // top-level range so their order stays deterministic (appended
        // after existing top-level folders, internal order preserved).
        let base: i64 = {
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(position), -1) FROM session_folders \
                     WHERE parent_id IS NULL",
                    (),
                )
                .await
                .map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql top-level max query: {e}"))
                })?;
            match rows.next().await.map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql top-level max row: {e}"))
            })? {
                Some(row) => row.get(0).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql top-level max: {e}"))
                })?,
                None => -1,
            }
        };
        tx.execute(
            "UPDATE session_folders SET parent_id = NULL, position = position + ?2 \
             WHERE parent_id = ?1",
            libsql::params![id_str.clone(), base + 1],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql promote sub-folders: {e}")))?;

        // Null the direct chats back to uncategorized — never delete them.
        tx.execute(
            "UPDATE sessions SET folder_id = NULL WHERE folder_id = ?1",
            libsql::params![id_str.clone()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql clear folder members: {e}")))?;

        let affected = tx
            .execute(
                "DELETE FROM session_folders WHERE id = ?1",
                libsql::params![id_str.clone()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete folder: {e}")))?;
        if affected == 0 {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        tx.commit().await.map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql commit delete-folder: {e}"))
        })?;
        Ok(Some(affected_sessions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libsql::LibsqlSessionStore;
    use baybo_model::{ChannelType, Session, SessionState, TriggerSource, User};
    use baybo_store::SessionStore;

    fn folder_row(id: &str, parent: Option<&str>, name: &str, pos: i64) -> SessionFolderRow {
        SessionFolderRow {
            id: FolderId::from(id),
            parent_id: parent.map(FolderId::from),
            name: name.to_owned(),
            position: pos,
            created_at: chrono::Utc::now(),
        }
    }

    fn session(id: &str) -> Session {
        let sid = SessionId::from(id);
        Session {
            id: sid.clone(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::http(),
            },
            channel: ChannelType::http(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            state: SessionState::default(),
            root_session_id: sid,
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
        }
    }

    #[tokio::test]
    async fn create_list_rename() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionFolderStore::new(pool);
        store.create(&folder_row("a", None, "A", 0)).await.unwrap();
        store.create(&folder_row("b", None, "B", 1)).await.unwrap();
        store
            .create(&folder_row("a1", Some("a"), "A1", 0))
            .await
            .unwrap();

        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 3);

        assert!(store.rename(&FolderId::from("a"), "Alpha").await.unwrap());
        let a = store.get(&FolderId::from("a")).await.unwrap().unwrap();
        assert_eq!(a.name, "Alpha");

        // Unknown id reports no row touched.
        assert!(!store.rename(&FolderId::from("zzz"), "X").await.unwrap());
    }

    #[tokio::test]
    async fn reorder_renumbers_sibling_group() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionFolderStore::new(pool);
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            store
                .create(&folder_row(id, None, id, i as i64))
                .await
                .unwrap();
        }
        store
            .reorder(
                None,
                &[
                    FolderId::from("c"),
                    FolderId::from("a"),
                    FolderId::from("b"),
                ],
            )
            .await
            .unwrap();
        let pos = |all: &[SessionFolderRow], id: &str| {
            all.iter().find(|f| f.id.as_str() == id).unwrap().position
        };
        let all = store.list().await.unwrap();
        assert_eq!(pos(&all, "c"), 0);
        assert_eq!(pos(&all, "a"), 1);
        assert_eq!(pos(&all, "b"), 2);
    }

    #[tokio::test]
    async fn delete_dissolves_without_removing_sessions() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let folders = LibsqlSessionFolderStore::new(pool.clone());
        let sessions = LibsqlSessionStore::new(pool);

        // parent "p" with a sub-folder "c"; one chat filed directly under
        // each.
        folders
            .create(&folder_row("p", None, "P", 0))
            .await
            .unwrap();
        folders
            .create(&folder_row("c", Some("p"), "C", 0))
            .await
            .unwrap();
        let sp = session("chat-in-p");
        let sc = session("chat-in-c");
        sessions.save(&sp).await.unwrap();
        sessions.save(&sc).await.unwrap();
        sessions
            .set_folder(&sp.id, Some(&FolderId::from("p")))
            .await
            .unwrap();
        sessions
            .set_folder(&sc.id, Some(&FolderId::from("c")))
            .await
            .unwrap();

        let affected = folders
            .delete(&FolderId::from("p"))
            .await
            .unwrap()
            .expect("folder existed");
        assert_eq!(
            affected,
            vec![sp.id.clone()],
            "only p's direct chat cleared"
        );

        // p's direct chat fell back to uncategorized; c's chat untouched.
        assert_eq!(sessions.get(&sp.id).await.unwrap().unwrap().folder_id, None);
        assert_eq!(
            sessions.get(&sc.id).await.unwrap().unwrap().folder_id,
            Some(FolderId::from("c"))
        );
        // The sub-folder survived and was promoted to top-level.
        assert_eq!(
            folders
                .get(&FolderId::from("c"))
                .await
                .unwrap()
                .unwrap()
                .parent_id,
            None
        );
        // The deleted folder is gone; NO session rows were removed.
        assert!(folders.get(&FolderId::from("p")).await.unwrap().is_none());
        assert!(sessions.get(&sp.id).await.unwrap().is_some());
        assert!(sessions.get(&sc.id).await.unwrap().is_some());

        // Deleting an unknown folder reports no row.
        assert!(
            folders
                .delete(&FolderId::from("ghost"))
                .await
                .unwrap()
                .is_none()
        );
    }
}
