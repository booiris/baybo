//! sqlite implementation of [`SessionFolderStore`].

use async_trait::async_trait;
use baybo_model::{FolderId, SessionId};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::session_folder::{Result, SessionFolderRow, SessionFolderStore};

pub struct SqliteSessionFolderStore {
    pool: SqlitePool,
}

impl SqliteSessionFolderStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Raw column tuple: (id, parent_id, name, position, created_at µs).
type RawFolder = (String, Option<String>, String, i64, i64);

fn row_from_raw(raw: RawFolder) -> Result<SessionFolderRow> {
    let (id, parent_id, name, position, created_at_us) = raw;
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
impl SessionFolderStore for SqliteSessionFolderStore {
    async fn list(&self) -> Result<Vec<SessionFolderRow>> {
        let raws = self
            .pool
            .interact("session_folders.list", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, parent_id, name, position, created_at \
                     FROM session_folders ORDER BY parent_id, position, id",
                )?;
                let raws = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<RawFolder>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(row_from_raw).collect()
    }

    async fn get(&self, id: &FolderId) -> Result<Option<SessionFolderRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact("session_folders.get", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, parent_id, name, position, created_at \
                         FROM session_folders WHERE id = ?1",
                        rusqlite::params![id],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;
        match raw {
            Some(raw) => Ok(Some(row_from_raw(raw)?)),
            None => Ok(None),
        }
    }

    async fn create(&self, row: &SessionFolderRow) -> Result<()> {
        let id = row.id.as_str().to_string();
        let parent_id = row.parent_id.as_ref().map(|p| p.as_str().to_string());
        let name = row.name.clone();
        let position = row.position;
        let created_at = super::time::to_us(row.created_at);
        self.pool
            .interact("session_folders.create", move |conn| {
                conn.execute(
                    "INSERT INTO session_folders (id, parent_id, name, position, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id, parent_id, name, position, created_at],
                )?;
                Ok(())
            })
            .await
    }

    async fn rename(&self, id: &FolderId, name: &str) -> Result<bool> {
        let id = id.as_str().to_string();
        let name = name.to_string();
        let affected = self
            .pool
            .interact("session_folders.rename", move |conn| {
                Ok(conn.execute(
                    "UPDATE session_folders SET name = ?2 WHERE id = ?1",
                    rusqlite::params![id, name],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn reparent(
        &self,
        id: &FolderId,
        parent_id: Option<&FolderId>,
        position: i64,
    ) -> Result<bool> {
        let id = id.as_str().to_string();
        let parent_id = parent_id.map(|p| p.as_str().to_string());
        let affected = self
            .pool
            .interact("session_folders.reparent", move |conn| {
                Ok(conn.execute(
                    "UPDATE session_folders SET parent_id = ?2, position = ?3 WHERE id = ?1",
                    rusqlite::params![id, parent_id, position],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn reorder(&self, parent_id: Option<&FolderId>, ordered_ids: &[FolderId]) -> Result<()> {
        let parent = parent_id.map(|p| p.as_str().to_string());
        let ordered: Vec<String> = ordered_ids.iter().map(|i| i.as_str().to_string()).collect();
        self.pool
            .interact("session_folders.reorder", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                for (idx, id) in ordered.iter().enumerate() {
                    // `parent_id IS ?3` is NULL-safe: a NULL bind matches top-level
                    // rows, a value bind matches that exact parent — so a stray id
                    // from a different group can't be renumbered into this one.
                    tx.execute(
                        "UPDATE session_folders SET position = ?2 \
                         WHERE id = ?1 AND parent_id IS ?3",
                        rusqlite::params![id, idx as i64, parent],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    async fn delete(&self, id: &FolderId) -> Result<Option<Vec<SessionId>>> {
        let id_str = id.as_str().to_string();
        self.pool
            .interact("session_folders.delete", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // Sessions filed directly under this folder, collected before they
                // are nulled so the caller can broadcast their "now uncategorized"
                // patches for live convergence.
                let affected_sessions: Vec<SessionId> = {
                    let mut stmt = tx.prepare("SELECT id FROM sessions WHERE folder_id = ?1")?;
                    let ids = stmt
                        .query_map(rusqlite::params![id_str], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    ids.into_iter().map(SessionId::from).collect()
                };

                // Promote sub-folders to top-level, shifted above the current
                // top-level range so their order stays deterministic (appended
                // after existing top-level folders, internal order preserved).
                let base: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position), -1) FROM session_folders \
                     WHERE parent_id IS NULL",
                    [],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "UPDATE session_folders SET parent_id = NULL, position = position + ?2 \
                     WHERE parent_id = ?1",
                    rusqlite::params![id_str, base + 1],
                )?;

                // Null the direct chats back to uncategorized — never delete them.
                tx.execute(
                    "UPDATE sessions SET folder_id = NULL WHERE folder_id = ?1",
                    rusqlite::params![id_str],
                )?;

                let affected = tx.execute(
                    "DELETE FROM session_folders WHERE id = ?1",
                    rusqlite::params![id_str],
                )?;
                if affected == 0 {
                    drop(tx);
                    return Ok(None);
                }
                tx.commit()?;
                Ok(Some(affected_sessions))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteSessionStore;
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
                channel: ChannelType::owner(),
            },
            channel: ChannelType::owner(),
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
            title: None,
        }
    }

    #[tokio::test]
    async fn create_list_rename() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionFolderStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let store = SqliteSessionFolderStore::new(pool);
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
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let folders = SqliteSessionFolderStore::new(pool.clone());
        let sessions = SqliteSessionStore::new(pool);

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
