//! sqlite implementation of [`AgentProfileStore`].

use async_trait::async_trait;
use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, BUILTIN_AGENT_PROFILE_ID, LlmEntryName, ProjectId,
    TeamMembership,
};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate, Result};

/// Card text of the seeded built-in profile. Permanent copy: the builtin's
/// description is read-only, so this string is what the Agents card shows
/// for the lifetime of the install.
const BUILTIN_AGENT_PROFILE_DESCRIPTION: &str =
    "Baybo's default persona: workspace Soul prompt, default model, full skill and tool set.";

const SELECT_COLS: &str = "id, description, avatar_blob_id, framework, \
                           llm, builtin, project_id, handle, hired_by, \
                           deleted_at, created_at, updated_at";

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
                     (id, description, framework, builtin, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, 1, ?4, ?4)",
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

/// Map a sqlite write error to [`StorageError::Conflict`] when it tripped a
/// constraint, else a generic internal error. Same message-sniff as the
/// device store.
///
/// Two constraints reach this. `PRIMARY KEY(id)` is a backstop nothing
/// hits, since ids are freshly-minted ULIDs. `idx_agent_profiles_handle` is
/// live: two hires racing for `@dev-1` is exactly what it exists to refuse,
/// and the loser must see a conflict rather than an internal error.
fn write_conflict_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    let msg = e.to_string();
    if msg.contains("constraint") || msg.contains("UNIQUE") {
        StorageError::Conflict(format!("{ctx}: conflicting write"))
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
    Option<String>,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
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
        row.get(10)?,
        row.get(11)?,
    ))
}

fn row_from_raw(raw: RawProfileRow) -> Result<AgentProfileRow> {
    let (
        id,
        description,
        avatar_blob_id,
        framework_raw,
        llm,
        builtin_col,
        project_id,
        handle,
        hired_by,
        deleted_at_us,
        created_at_us,
        updated_at_us,
    ) = raw;
    let framework = AgentFramework::parse(&framework_raw).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.framework: unknown value {framework_raw:?}"
        ))
    })?;
    let stamp = |column: &str, us: i64| {
        super::time::from_us(us)
            .ok_or_else(|| StorageError::Storage(format!("{column} out of range: {us}")))
    };
    let created_at = stamp("agent_profiles.created_at", created_at_us)?;
    let updated_at = stamp("agent_profiles.updated_at", updated_at_us)?;
    let deleted_at = deleted_at_us
        .map(|us| stamp("agent_profiles.deleted_at", us))
        .transpose()?;
    // Both columns or neither: the pair is what `TeamMembership` means, and
    // a half-written membership is an agent the roster shows and nobody can
    // mention, or one that is mentionable and belongs to no board.
    let team = match (project_id, handle) {
        (Some(project_id), Some(handle)) => Some(TeamMembership {
            project_id: ProjectId::parse(project_id)
                .map_err(|e| StorageError::Storage(e.to_string()))?,
            handle: AgentHandle::parse(handle).map_err(|e| StorageError::Storage(e.to_string()))?,
        }),
        (None, None) => None,
        (project_id, handle) => {
            return Err(StorageError::Storage(format!(
                "agent_profiles: half a team membership (project_id={project_id:?}, \
                 handle={handle:?})"
            )));
        }
    };
    Ok(AgentProfileRow {
        // A stored id that fails the grammar is a hard error, not a warn:
        // this id names the profile's persona directory, and every consumer
        // of the row joins it back onto the filesystem.
        id: AgentProfileId::parse(id).map_err(|e| StorageError::Storage(e.to_string()))?,
        description,
        avatar_blob_id,
        framework,
        llm: llm.map(LlmEntryName::from),
        builtin: builtin_col != 0,
        team,
        hired_by: hired_by
            .map(AgentProfileId::parse)
            .transpose()
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        deleted_at,
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
                    // The scope filter is in the statement so no caller can
                    // forget it and leak somebody's teammate into the global
                    // roster. Team members leave through their board.
                    "SELECT {SELECT_COLS} FROM agent_profiles \
                     WHERE project_id IS NULL \
                     ORDER BY builtin DESC, id"
                ))?;
                let raws = stmt
                    .query_map([], read_raw_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(row_from_raw).collect()
    }

    async fn list_team(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>> {
        let project = project.as_str().to_string();
        let raws = self
            .pool
            .interact("agent_profiles.list_team", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM agent_profiles \
                     WHERE project_id = ?1 AND deleted_at IS NULL \
                     ORDER BY handle"
                ))?;
                let raws = stmt
                    .query_map(rusqlite::params![project], read_raw_row)?
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
        let id = row.id.as_str().to_string();
        let description = row.description.clone();
        let avatar_blob_id = row.avatar_blob_id.clone();
        let framework = row.framework.as_str();
        let llm = row.llm.as_ref().map(|l| l.as_str().to_string());
        let project_id = row.team.as_ref().map(|t| t.project_id.as_str().to_string());
        let handle = row.team.as_ref().map(|t| t.handle.as_str().to_string());
        let hired_by = row.hired_by.as_ref().map(|id| id.as_str().to_string());
        let created_at = super::time::to_us(row.created_at);
        let updated_at = super::time::to_us(row.updated_at);
        // The write error has to survive the closure as data: `Conflict` is a
        // non-`Internal` variant and can't be built inside it.
        let outcome = self
            .pool
            .interact("agent_profiles.create", move |conn| {
                // Neither `builtin` nor `deleted_at` is in the column list:
                // the schema default fills the first (so the seed stays the
                // only writer of 1) and an agent is never born removed.
                match conn.execute(
                    "INSERT INTO agent_profiles \
                     (id, description, avatar_blob_id, framework, \
                      llm, project_id, handle, hired_by, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        id,
                        description,
                        avatar_blob_id,
                        framework,
                        llm,
                        project_id,
                        handle,
                        hired_by,
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
            Some(e) => Err(write_conflict_err("create agent profile", e)),
        }
    }

    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let id = id.as_str().to_string();
        let description = update.description.clone();
        let framework = update.framework.as_str();
        let now = super::time::now_us();
        let outcome = self
            .pool
            .interact("agent_profiles.update", move |conn| {
                match conn.execute(
                    // The self-reference on the builtin's `framework` is the
                    // guard: no caller can move it off baybo, while every
                    // other row takes the requested value.
                    "UPDATE agent_profiles SET \
                     description = ?2, \
                     framework = CASE WHEN builtin = 1 THEN framework ELSE ?3 END, \
                     updated_at = ?4 \
                     WHERE id = ?1",
                    rusqlite::params![id, description, framework, now],
                ) {
                    Ok(affected) => Ok(Ok(affected)),
                    Err(e) => Ok(Err(e.to_string())),
                }
            })
            .await?;
        match outcome {
            Ok(affected) => Ok(affected > 0),
            Err(e) => Err(write_conflict_err("update agent profile", e)),
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

    async fn set_llm(&self, id: &AgentProfileId, llm: Option<&LlmEntryName>) -> Result<bool> {
        let id = id.as_str().to_string();
        let llm = llm.map(|l| l.as_str().to_string());
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("agent_profiles.set_llm", move |conn| {
                Ok(conn.execute(
                    // The builtin follows `default-llm` by definition, so its
                    // pin is forced empty rather than merely left alone —
                    // that also clears anything an earlier build stored.
                    "UPDATE agent_profiles SET \
                     llm = CASE WHEN builtin = 1 THEN NULL ELSE ?2 END, \
                     updated_at = ?3 \
                     WHERE id = ?1",
                    rusqlite::params![id, llm, now],
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
                    // The team guard is structural for the same reason the
                    // builtin one is: a row an issue's `assignee` points at
                    // must not be reachable by the global delete path.
                    "DELETE FROM agent_profiles \
                     WHERE id = ?1 AND builtin = 0 AND project_id IS NULL",
                    rusqlite::params![id],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn remove_from_team(&self, id: &AgentProfileId) -> Result<bool> {
        let id = id.as_str().to_string();
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("agent_profiles.remove_from_team", move |conn| {
                Ok(conn.execute(
                    // `deleted_at IS NULL` keeps the stamp honest: a second
                    // removal must not rewrite when the agent actually left.
                    "UPDATE agent_profiles SET deleted_at = ?2, updated_at = ?2 \
                     WHERE id = ?1 AND project_id IS NOT NULL AND deleted_at IS NULL",
                    rusqlite::params![id, now],
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

    fn custom_row() -> AgentProfileRow {
        let now = now_us_precision();
        AgentProfileRow {
            id: AgentProfileId::generate(),
            description: "a test persona".to_owned(),
            avatar_blob_id: None,
            framework: AgentFramework::Claude,
            llm: Some(LlmEntryName::from("primary")),
            builtin: false,
            team: None,
            hired_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn project(name: &str) -> ProjectId {
        ProjectId::parse(name.to_owned()).expect("valid project id")
    }

    fn team_row(project: &ProjectId, handle: &str) -> AgentProfileRow {
        AgentProfileRow {
            framework: AgentFramework::Baybo,
            team: Some(TeamMembership {
                project_id: project.clone(),
                handle: AgentHandle::parse(handle.to_owned()).expect("valid handle"),
            }),
            ..custom_row()
        }
    }

    fn content_update() -> AgentProfileUpdate {
        AgentProfileUpdate {
            description: String::new(),
            framework: AgentFramework::Baybo,
        }
    }

    #[tokio::test]
    async fn open_seeds_locked_builtin_defaults() {
        let store = open_store().await;
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        let b = &rows[0];
        assert_eq!(b.id.as_str(), BUILTIN_AGENT_PROFILE_ID);
        assert_eq!(b.id, AgentProfileId::builtin());
        assert_eq!(b.description, BUILTIN_AGENT_PROFILE_DESCRIPTION);
        assert!(b.builtin);
        assert_eq!(b.framework, AgentFramework::Baybo);
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
        let mut row = custom_row();
        row.builtin = true; // must be ignored by the insert
        store.create(&row).await.unwrap();

        let back = store.get(&row.id).await.unwrap().unwrap();
        assert!(!back.builtin, "create must never mint a builtin row");
        assert_eq!(back.framework, AgentFramework::Claude);
        assert_eq!(back.llm, Some(LlmEntryName::from("primary")));
        assert_eq!(back.created_at, row.created_at);
    }

    #[tokio::test]
    async fn update_full_replaces_content() {
        let store = open_store().await;
        let row = custom_row();
        store.create(&row).await.unwrap();

        // Full replace resets every optional field to the update's state.
        assert!(store.update(&row.id, &content_update()).await.unwrap());
        let back = store.get(&row.id).await.unwrap().unwrap();
        assert_eq!(back.description, "");
        assert_eq!(back.framework, AgentFramework::Baybo);
        // The pin is not part of the full replace any more — it has its own
        // setter, so a content update leaves it alone.
        assert_eq!(back.llm, row.llm);
        assert!(back.updated_at >= back.created_at);

        // Missing rows are the only `Ok(false)` — the builtin has its own
        // test above, since it is reachable now except for its framework.
        assert!(
            !store
                .update(
                    &AgentProfileId::parse("missing").expect("valid id"),
                    &content_update()
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn delete_skips_builtin_and_removes_customs() {
        let store = open_store().await;
        let row = custom_row();
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

    /// The two fields the builtin may change each have a setter that skips
    /// the `builtin = 0` guard — which is what keeps the lock structural
    /// instead of per-field validation one layer up.
    /// The builtin's description is ordinary editable text; only its
    /// framework is pinned, and pinned by the statement rather than by a
    /// caller remembering to ask nicely.
    #[tokio::test]
    async fn update_reaches_the_builtin_but_never_moves_its_framework() {
        let store = open_store().await;
        let builtin = AgentProfileId::builtin();

        assert!(
            store
                .update(
                    &builtin,
                    &AgentProfileUpdate {
                        description: "my own words".to_owned(),
                        framework: AgentFramework::Claude,
                    },
                )
                .await
                .unwrap()
        );
        let back = store.get(&builtin).await.unwrap().unwrap();
        assert_eq!(back.description, "my own words");
        assert_eq!(
            back.framework,
            AgentFramework::Baybo,
            "the builtin runs on baybo by definition"
        );

        // …and it still cannot be deleted.
        assert!(!store.delete(&builtin).await.unwrap());
    }

    /// A custom agent's pin is its own; the builtin's is always empty,
    /// because that row *is* `default-llm`.
    #[tokio::test]
    async fn set_llm_pins_a_custom_agent_and_never_the_builtin() {
        let store = open_store().await;
        let pin = LlmEntryName::from("fast");

        let row = custom_row();
        store.create(&row).await.unwrap();
        assert!(store.set_llm(&row.id, Some(&pin)).await.unwrap());
        assert_eq!(
            store.get(&row.id).await.unwrap().unwrap().llm,
            Some(pin.clone())
        );
        assert!(store.set_llm(&row.id, None).await.unwrap());
        assert!(store.get(&row.id).await.unwrap().unwrap().llm.is_none());

        // The builtin absorbs the write and stays unpinned — which also
        // normalises a row an earlier build allowed to drift.
        let builtin = AgentProfileId::builtin();
        assert!(store.set_llm(&builtin, Some(&pin)).await.unwrap());
        assert!(store.get(&builtin).await.unwrap().unwrap().llm.is_none());
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
                .set_avatar(&AgentProfileId::parse("missing").expect("valid id"), None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_global_roster_and_a_team_roster_never_overlap() {
        let store = open_store().await;
        let alpha = project("alpha");
        let beta = project("beta");
        let global = custom_row();
        store.create(&global).await.unwrap();
        store.create(&team_row(&alpha, "lead")).await.unwrap();
        store.create(&team_row(&alpha, "dev-1")).await.unwrap();
        store.create(&team_row(&beta, "lead")).await.unwrap();

        let global_ids: Vec<AgentProfileId> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(global_ids, vec![AgentProfileId::builtin(), global.id]);

        let handles: Vec<String> = store
            .list_team(&alpha)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|r| r.team.map(|t| t.handle.as_str().to_owned()))
            .collect();
        assert_eq!(handles, vec!["dev-1", "lead"], "ordered by handle");
    }

    #[tokio::test]
    async fn a_handle_is_unique_within_its_board_and_stays_reserved() {
        let store = open_store().await;
        let alpha = project("alpha");
        store.create(&team_row(&alpha, "lead")).await.unwrap();

        let clash = store.create(&team_row(&alpha, "lead")).await;
        assert!(
            matches!(clash, Err(StorageError::Conflict(_))),
            "a duplicate handle is a conflict, not an internal error: {clash:?}"
        );

        let leaving = store.list_team(&alpha).await.unwrap()[0].id.clone();
        assert!(store.remove_from_team(&leaving).await.unwrap());
        assert!(matches!(
            store.create(&team_row(&alpha, "lead")).await,
            Err(StorageError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn a_removed_teammate_leaves_the_roster_and_stays_resolvable() {
        let store = open_store().await;
        let alpha = project("alpha");
        let row = team_row(&alpha, "dev-1");
        store.create(&row).await.unwrap();

        assert!(store.remove_from_team(&row.id).await.unwrap());
        assert!(store.list_team(&alpha).await.unwrap().is_empty());
        let back = store.get(&row.id).await.unwrap().expect("row survives");
        assert!(back.deleted_at.is_some());
        assert_eq!(
            back.team.clone().map(|t| t.handle.as_str().to_owned()),
            Some("dev-1".to_owned())
        );

        assert!(!store.remove_from_team(&row.id).await.unwrap());
        let again = store.get(&row.id).await.unwrap().expect("row survives");
        assert_eq!(again.deleted_at, back.deleted_at);
    }

    #[tokio::test]
    async fn delete_refuses_a_team_member() {
        let store = open_store().await;
        let row = team_row(&project("alpha"), "dev-1");
        store.create(&row).await.unwrap();

        assert!(!store.delete(&row.id).await.unwrap());
        assert!(store.get(&row.id).await.unwrap().is_some());
        assert!(store.remove_from_team(&row.id).await.unwrap());
        assert!(!store.delete(&row.id).await.unwrap());
        assert!(store.get(&row.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_hire_records_who_hired_it() {
        let store = open_store().await;
        let alpha = project("alpha");
        let lead = team_row(&alpha, "lead");
        store.create(&lead).await.unwrap();
        let hire = AgentProfileRow {
            hired_by: Some(lead.id.clone()),
            ..team_row(&alpha, "dev-1")
        };
        store.create(&hire).await.unwrap();

        assert_eq!(
            store.get(&hire.id).await.unwrap().unwrap().hired_by,
            Some(lead.id.clone())
        );
        assert_eq!(
            store.get(&lead.id).await.unwrap().unwrap().hired_by,
            None,
            "the operator's own creations name nobody"
        );
    }

    /// The store no longer knows a display name, so ordering it can only
    /// promise the builtin first and a stable tail — the gateway sorts by
    /// the name it derives from each agent's `IDENTITY.md`.
    #[tokio::test]
    async fn list_puts_the_builtin_first_and_is_otherwise_stable() {
        let store = open_store().await;
        store.create(&custom_row()).await.unwrap();
        store.create(&custom_row()).await.unwrap();
        let ids: Vec<AgentProfileId> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], AgentProfileId::builtin());
        let mut tail = ids[1..].to_vec();
        tail.sort();
        assert_eq!(tail, ids[1..], "the tail must be id-ordered");
    }
}
