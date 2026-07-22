//! sqlite implementation of [`AgentProfileStore`].

use async_trait::async_trait;
use baybo_model::{AgentFramework, AgentProfileId, BUILTIN_AGENT_PROFILE_ID, LlmEntryName};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate, Result};

/// Card text of the seeded built-in profile. Permanent copy: the builtin's
/// description is read-only, so this string is what the Agents card shows
/// for the lifetime of the install.
const BUILTIN_AGENT_PROFILE_DESCRIPTION: &str =
    "Baybo's default persona: workspace Soul prompt, default model, full skill and tool set.";

const SELECT_COLS: &str = "id, name, description, avatar_blob_id, system_prompt, framework, \
                           llm, builtin, created_at, updated_at";

pub struct SqliteAgentProfileStore {
    pool: SqlitePool,
}

impl SqliteAgentProfileStore {
    /// Open the store and seed the built-in `baybo` row. `INSERT OR IGNORE`
    /// gives a fresh DB the row and leaves an existing one untouched
    /// (including a user-set avatar). This seed is the only statement in
    /// the process that writes `builtin = 1`.
    pub async fn open(pool: SqlitePool) -> anyhow::Result<Self> {
        let store = Self { pool };
        let now = super::time::now_us();
        store
            .pool
            .interact("agent_profiles.seed_builtin", move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO agent_profiles \
                     (id, name, description, framework, builtin, created_at, updated_at) \
                     VALUES (?1, ?1, ?2, ?3, 1, ?4, ?4)",
                    rusqlite::params![
                        BUILTIN_AGENT_PROFILE_ID,
                        BUILTIN_AGENT_PROFILE_DESCRIPTION,
                        AgentFramework::Baybo.as_str(),
                        now,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to seed builtin agent profile: {e}"))?;
        Ok(store)
    }
}

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("sqlite {ctx}: {e}"))
}

/// Map a sqlite write error to [`StorageError::Conflict`] when it tripped
/// the case-insensitive `UNIQUE` on `agent_profiles.name`, else a generic
/// internal error. Same message-sniff as the device store.
///
/// The sniff assumes a constraint trip is the name `UNIQUE`: the only other
/// constraint on the table is `PRIMARY KEY(id)`, and ids are freshly-minted
/// ULIDs, so a PK collision is astronomically unlikely.
fn name_conflict_err(ctx: &str, name: &str, e: impl std::fmt::Display) -> StorageError {
    let msg = e.to_string();
    if msg.contains("constraint") || msg.contains("UNIQUE") {
        StorageError::Conflict(format!("an agent named {name:?} already exists"))
    } else {
        col_err(ctx, e)
    }
}

/// The `SELECT_COLS` columns exactly as sqlite hands them over. Decoding into
/// an [`AgentProfileRow`] can fail with a non-`Internal` [`StorageError`], which
/// cannot be built inside the `interact` closure — so the raw columns come out
/// first and are decoded afterwards.
type RawProfileRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    i64,
    i64,
    i64,
);

fn read_raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawProfileRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn row_from_raw(raw: RawProfileRow) -> Result<AgentProfileRow> {
    let (
        id,
        name,
        description,
        avatar_blob_id,
        system_prompt,
        framework_raw,
        llm,
        builtin_col,
        created_at_us,
        updated_at_us,
    ) = raw;
    let framework = AgentFramework::parse(&framework_raw).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.framework: unknown value {framework_raw:?}"
        ))
    })?;
    let created_at = super::time::from_us(created_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.created_at out of range: {created_at_us}"
        ))
    })?;
    let updated_at = super::time::from_us(updated_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.updated_at out of range: {updated_at_us}"
        ))
    })?;
    Ok(AgentProfileRow {
        id: AgentProfileId::from(id),
        name,
        description,
        avatar_blob_id,
        system_prompt,
        framework,
        llm: llm.map(LlmEntryName::from),
        builtin: builtin_col != 0,
        created_at,
        updated_at,
    })
}

#[async_trait]
impl AgentProfileStore for SqliteAgentProfileStore {
    async fn list(&self) -> Result<Vec<AgentProfileRow>> {
        let raws = self
            .pool
            .interact("agent_profiles.list", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM agent_profiles \
                     ORDER BY builtin DESC, name COLLATE NOCASE, id"
                ))?;
                let raws = stmt
                    .query_map([], read_raw_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(row_from_raw).collect()
    }

    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact("agent_profiles.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {SELECT_COLS} FROM agent_profiles WHERE id = ?1"),
                        rusqlite::params![id],
                        read_raw_row,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(row_from_raw).transpose()
    }

    async fn create(&self, row: &AgentProfileRow) -> Result<()> {
        let name = row.name.clone();
        let id = row.id.as_str().to_string();
        let description = row.description.clone();
        let avatar_blob_id = row.avatar_blob_id.clone();
        let system_prompt = row.system_prompt.clone();
        let framework = row.framework.as_str();
        let llm = row.llm.as_ref().map(|l| l.as_str().to_string());
        let created_at = super::time::to_us(row.created_at);
        let updated_at = super::time::to_us(row.updated_at);
        let insert_name = name.clone();
        // The write error has to survive the closure as data: `Conflict` is a
        // non-`Internal` variant and can't be built inside it.
        let outcome = self
            .pool
            .interact("agent_profiles.create", move |conn| {
                // `builtin` is deliberately not in the column list: the schema
                // DEFAULT 0 fills it, so the seed stays the only writer of 1.
                match conn.execute(
                    "INSERT INTO agent_profiles \
                     (id, name, description, avatar_blob_id, system_prompt, framework, \
                      llm, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id,
                        insert_name,
                        description,
                        avatar_blob_id,
                        system_prompt,
                        framework,
                        llm,
                        created_at,
                        updated_at,
                    ],
                ) {
                    Ok(_) => Ok(None),
                    Err(e) => Ok(Some(e.to_string())),
                }
            })
            .await?;
        match outcome {
            None => Ok(()),
            Some(e) => Err(name_conflict_err("create agent profile", &name, e)),
        }
    }

    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let id = id.as_str().to_string();
        let name = update.name.clone();
        let update_name = name.clone();
        let description = update.description.clone();
        let system_prompt = update.system_prompt.clone();
        let framework = update.framework.as_str();
        let llm = update.llm.as_ref().map(|l| l.as_str().to_string());
        let now = super::time::now_us();
        let outcome = self
            .pool
            .interact("agent_profiles.update", move |conn| {
                match conn.execute(
                    "UPDATE agent_profiles SET \
                     name = ?2, description = ?3, system_prompt = ?4, framework = ?5, \
                     llm = ?6, updated_at = ?7 \
                     WHERE id = ?1 AND builtin = 0",
                    rusqlite::params![
                        id,
                        update_name,
                        description,
                        system_prompt,
                        framework,
                        llm,
                        now,
                    ],
                ) {
                    Ok(affected) => Ok(Ok(affected)),
                    Err(e) => Ok(Err(e.to_string())),
                }
            })
            .await?;
        match outcome {
            Ok(affected) => Ok(affected > 0),
            Err(e) => Err(name_conflict_err("update agent profile", &name, e)),
        }
    }

    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool> {
        let id = id.as_str().to_string();
        let blob_id = blob_id.map(str::to_string);
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("agent_profiles.set_avatar", move |conn| {
                Ok(conn.execute(
                    "UPDATE agent_profiles SET avatar_blob_id = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, blob_id, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn delete(&self, id: &AgentProfileId) -> Result<bool> {
        let id = id.as_str().to_string();
        let affected = self
            .pool
            .interact("agent_profiles.delete", move |conn| {
                Ok(conn.execute(
                    "DELETE FROM agent_profiles WHERE id = ?1 AND builtin = 0",
                    rusqlite::params![id],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    async fn open_store() -> SqliteAgentProfileStore {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        SqliteAgentProfileStore::open(pool).await.unwrap()
    }

    fn now_us_precision() -> DateTime<Utc> {
        crate::sqlite::time::from_us(crate::sqlite::time::now_us()).unwrap()
    }

    fn custom_row(name: &str) -> AgentProfileRow {
        let now = now_us_precision();
        AgentProfileRow {
            id: AgentProfileId::generate(),
            name: name.to_owned(),
            description: "a test persona".to_owned(),
            avatar_blob_id: None,
            system_prompt: Some("You are terse.".to_owned()),
            framework: AgentFramework::Claude,
            llm: Some(LlmEntryName::from("primary")),
            builtin: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn content_update(name: &str) -> AgentProfileUpdate {
        AgentProfileUpdate {
            name: name.to_owned(),
            description: String::new(),
            system_prompt: None,
            framework: AgentFramework::Baybo,
            llm: None,
        }
    }

    #[tokio::test]
    async fn open_seeds_locked_builtin_defaults() {
        let store = open_store().await;
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        let b = &rows[0];
        assert_eq!(b.id.as_str(), BUILTIN_AGENT_PROFILE_ID);
        assert_eq!(b.name, "baybo");
        assert_eq!(b.description, BUILTIN_AGENT_PROFILE_DESCRIPTION);
        assert!(b.builtin);
        assert_eq!(b.framework, AgentFramework::Baybo);
        assert!(b.system_prompt.is_none());
        assert!(b.llm.is_none());
        assert!(b.avatar_blob_id.is_none());
    }

    #[tokio::test]
    async fn reseed_preserves_builtin_avatar() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteAgentProfileStore::open(pool.clone()).await.unwrap();
        let builtin = AgentProfileId::builtin();
        assert!(
            store
                .set_avatar(&builtin, Some("sha256:aa.bb"))
                .await
                .unwrap()
        );

        let store = SqliteAgentProfileStore::open(pool).await.unwrap();
        let row = store.get(&builtin).await.unwrap().unwrap();
        assert_eq!(row.avatar_blob_id.as_deref(), Some("sha256:aa.bb"));
    }

    #[tokio::test]
    async fn create_get_round_trips_and_never_binds_builtin() {
        let store = open_store().await;
        let mut row = custom_row("Reviewer");
        row.builtin = true; // must be ignored by the insert
        store.create(&row).await.unwrap();

        let back = store.get(&row.id).await.unwrap().unwrap();
        assert!(!back.builtin, "create must never mint a builtin row");
        assert_eq!(back.name, "Reviewer");
        assert_eq!(back.framework, AgentFramework::Claude);
        assert_eq!(back.llm, Some(LlmEntryName::from("primary")));
        assert_eq!(back.system_prompt.as_deref(), Some("You are terse."));
        assert_eq!(back.created_at, row.created_at);
    }

    #[tokio::test]
    async fn duplicate_name_is_case_insensitive_conflict() {
        let store = open_store().await;
        store.create(&custom_row("Helper")).await.unwrap();
        let err = store.create(&custom_row("hElPeR")).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "got {err:?}");

        // The builtin's name is reserved too.
        let err = store.create(&custom_row("Baybo")).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn update_full_replaces_content_and_skips_builtin() {
        let store = open_store().await;
        let row = custom_row("Helper");
        store.create(&row).await.unwrap();

        // Full replace resets every optional field to the update's state.
        assert!(
            store
                .update(&row.id, &content_update("Helper 2"))
                .await
                .unwrap()
        );
        let back = store.get(&row.id).await.unwrap().unwrap();
        assert_eq!(back.name, "Helper 2");
        assert_eq!(back.description, "");
        assert!(back.system_prompt.is_none());
        assert_eq!(back.framework, AgentFramework::Baybo);
        assert!(back.llm.is_none());
        assert!(back.updated_at >= back.created_at);

        // Builtin is unreachable behind the guard.
        let builtin = AgentProfileId::builtin();
        assert!(
            !store
                .update(&builtin, &content_update("renamed"))
                .await
                .unwrap()
        );
        // Missing rows are indistinguishable at the store layer.
        assert!(
            !store
                .update(&AgentProfileId::from("missing"), &content_update("x"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn rename_conflicts_only_against_other_rows() {
        let store = open_store().await;
        let a = custom_row("Alpha");
        let b = custom_row("Beta");
        store.create(&a).await.unwrap();
        store.create(&b).await.unwrap();

        // Case-only self-rename is fine.
        assert!(store.update(&a.id, &content_update("ALPHA")).await.unwrap());
        // Renaming onto another row's name (any casing) conflicts.
        let err = store
            .update(&b.id, &content_update("alpha"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn delete_skips_builtin_and_removes_customs() {
        let store = open_store().await;
        let row = custom_row("Helper");
        store.create(&row).await.unwrap();

        assert!(!store.delete(&AgentProfileId::builtin()).await.unwrap());
        assert!(
            store
                .get(&AgentProfileId::builtin())
                .await
                .unwrap()
                .is_some()
        );

        assert!(store.delete(&row.id).await.unwrap());
        assert!(store.get(&row.id).await.unwrap().is_none());
        assert!(!store.delete(&row.id).await.unwrap());
    }

    #[tokio::test]
    async fn set_avatar_reaches_builtin_and_clears() {
        let store = open_store().await;
        let builtin = AgentProfileId::builtin();
        assert!(
            store
                .set_avatar(&builtin, Some("sha256:cc.dd"))
                .await
                .unwrap()
        );
        let row = store.get(&builtin).await.unwrap().unwrap();
        assert_eq!(row.avatar_blob_id.as_deref(), Some("sha256:cc.dd"));

        assert!(store.set_avatar(&builtin, None).await.unwrap());
        let row = store.get(&builtin).await.unwrap().unwrap();
        assert!(row.avatar_blob_id.is_none());

        assert!(
            !store
                .set_avatar(&AgentProfileId::from("missing"), None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn list_orders_builtin_first_then_name_nocase() {
        let store = open_store().await;
        store.create(&custom_row("zeta")).await.unwrap();
        store.create(&custom_row("Alpha")).await.unwrap();
        let names: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["baybo", "Alpha", "zeta"]);
    }
}
