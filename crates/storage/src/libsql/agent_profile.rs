//! libsql implementation of [`AgentProfileStore`].

use async_trait::async_trait;
use baybo_model::{
    AgentFramework, AgentProfileId, BUILTIN_AGENT_PROFILE_ID, LlmEntryName, ReasoningEffort,
};

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate, Result};

/// Card text of the seeded built-in profile. Permanent copy: the builtin's
/// description is read-only, so this string is what the Agents card shows
/// for the lifetime of the install.
const BUILTIN_AGENT_PROFILE_DESCRIPTION: &str =
    "Baybo's default persona: workspace Soul prompt, default model, full skill and tool set.";

const SELECT_COLS: &str = "id, name, description, avatar_blob_id, system_prompt, framework, \
                           llm, builtin, created_at, updated_at, allowed_models, \
                           reasoning_effort";

pub struct LibsqlAgentProfileStore {
    pool: LibsqlPool,
}

impl LibsqlAgentProfileStore {
    /// Open the store and seed the built-in `baybo` row. `INSERT OR IGNORE`
    /// gives a fresh DB the row and leaves an existing one untouched
    /// (including a user-set avatar). This seed is the only statement in
    /// the process that writes `builtin = 1`.
    pub async fn open(pool: LibsqlPool) -> anyhow::Result<Self> {
        let store = Self { pool };
        let now = super::time::now_us();
        store
            .pool
            .conn()
            .execute(
                "INSERT OR IGNORE INTO agent_profiles \
                 (id, name, description, framework, builtin, created_at, updated_at) \
                 VALUES (?1, ?1, ?2, ?3, 1, ?4, ?4)",
                libsql::params![
                    BUILTIN_AGENT_PROFILE_ID,
                    BUILTIN_AGENT_PROFILE_DESCRIPTION,
                    AgentFramework::Baybo.as_str(),
                    now,
                ],
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to seed builtin agent profile: {e}"))?;
        Ok(store)
    }
}

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

/// Map a libsql write error to [`StorageError::Conflict`] when it tripped
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

fn row_from_libsql(row: &libsql::Row) -> Result<AgentProfileRow> {
    let id: String = row.get(0).map_err(|e| col_err("agent_profiles.id", e))?;
    let name: String = row.get(1).map_err(|e| col_err("agent_profiles.name", e))?;
    let description: String = row
        .get(2)
        .map_err(|e| col_err("agent_profiles.description", e))?;
    let avatar_blob_id: Option<String> = row
        .get(3)
        .map_err(|e| col_err("agent_profiles.avatar_blob_id", e))?;
    let system_prompt: Option<String> = row
        .get(4)
        .map_err(|e| col_err("agent_profiles.system_prompt", e))?;
    let framework_raw: String = row
        .get(5)
        .map_err(|e| col_err("agent_profiles.framework", e))?;
    let framework = AgentFramework::parse(&framework_raw).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.framework: unknown value {framework_raw:?}"
        ))
    })?;
    let llm: Option<String> = row.get(6).map_err(|e| col_err("agent_profiles.llm", e))?;
    let builtin_col: i64 = row
        .get(7)
        .map_err(|e| col_err("agent_profiles.builtin", e))?;
    let created_at_us: i64 = row
        .get(8)
        .map_err(|e| col_err("agent_profiles.created_at", e))?;
    let created_at = super::time::from_us(created_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.created_at out of range: {created_at_us}"
        ))
    })?;
    let updated_at_us: i64 = row
        .get(9)
        .map_err(|e| col_err("agent_profiles.updated_at", e))?;
    let updated_at = super::time::from_us(updated_at_us).ok_or_else(|| {
        StorageError::Storage(format!(
            "agent_profiles.updated_at out of range: {updated_at_us}"
        ))
    })?;
    let allowed_models_raw: Option<String> = row
        .get(10)
        .map_err(|e| col_err("agent_profiles.allowed_models", e))?;
    let allowed_models: Vec<LlmEntryName> = match allowed_models_raw {
        None => Vec::new(),
        Some(json) => {
            let names: Vec<String> = serde_json::from_str(&json).map_err(|e| {
                StorageError::Storage(format!("agent_profiles.allowed_models: bad JSON: {e}"))
            })?;
            names.into_iter().map(LlmEntryName::from).collect()
        }
    };
    let reasoning_effort_raw: Option<String> = row
        .get(11)
        .map_err(|e| col_err("agent_profiles.reasoning_effort", e))?;
    let reasoning_effort = match reasoning_effort_raw {
        None => None,
        Some(s) => Some(ReasoningEffort::parse(&s).ok_or_else(|| {
            StorageError::Storage(format!(
                "agent_profiles.reasoning_effort: unknown value {s:?}"
            ))
        })?),
    };
    Ok(AgentProfileRow {
        id: AgentProfileId::from(id),
        name,
        description,
        avatar_blob_id,
        system_prompt,
        framework,
        llm: llm.map(LlmEntryName::from),
        allowed_models,
        reasoning_effort,
        builtin: builtin_col != 0,
        created_at,
        updated_at,
    })
}

/// JSON-encode a non-empty allowed-model set as a `TEXT` column; an empty
/// set stores `NULL` (never `"[]"`) so "unrestricted" round-trips the same
/// way it started.
fn encode_allowed_models(models: &[LlmEntryName]) -> Result<Option<String>> {
    if models.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = models.iter().map(LlmEntryName::as_str).collect();
    serde_json::to_string(&names)
        .map(Some)
        .map_err(|e| StorageError::Storage(format!("encode allowed_models: {e}")))
}

#[async_trait]
impl AgentProfileStore for LibsqlAgentProfileStore {
    async fn list(&self) -> Result<Vec<AgentProfileRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {SELECT_COLS} FROM agent_profiles \
                     ORDER BY builtin DESC, name COLLATE NOCASE, id"
                ),
                (),
            )
            .await
            .map_err(|e| col_err("list agent profiles", e))?;
        let mut profiles = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| col_err("agent profile row", e))?
        {
            profiles.push(row_from_libsql(&row)?);
        }
        Ok(profiles)
    }

    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {SELECT_COLS} FROM agent_profiles WHERE id = ?1"),
                libsql::params![id.as_str().to_string()],
            )
            .await
            .map_err(|e| col_err("get agent profile", e))?;
        match rows
            .next()
            .await
            .map_err(|e| col_err("agent profile row", e))?
        {
            Some(row) => Ok(Some(row_from_libsql(&row)?)),
            None => Ok(None),
        }
    }

    async fn create(&self, row: &AgentProfileRow) -> Result<()> {
        let conn = self.pool.conn();
        // `builtin` is deliberately not in the column list: the schema
        // DEFAULT 0 fills it, so the seed stays the only writer of 1.
        let allowed_models = encode_allowed_models(&row.allowed_models)?;
        conn.execute(
            "INSERT INTO agent_profiles \
             (id, name, description, avatar_blob_id, system_prompt, framework, \
              llm, created_at, updated_at, allowed_models, reasoning_effort) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            libsql::params![
                row.id.as_str().to_string(),
                row.name.clone(),
                row.description.clone(),
                row.avatar_blob_id.clone(),
                row.system_prompt.clone(),
                row.framework.as_str(),
                row.llm.as_ref().map(|l| l.as_str().to_string()),
                super::time::to_us(row.created_at),
                super::time::to_us(row.updated_at),
                allowed_models,
                row.reasoning_effort.map(|e| e.as_str().to_owned()),
            ],
        )
        .await
        .map_err(|e| name_conflict_err("create agent profile", &row.name, e))?;
        Ok(())
    }

    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let conn = self.pool.conn();
        let allowed_models = encode_allowed_models(&update.allowed_models)?;
        let affected = conn
            .execute(
                "UPDATE agent_profiles SET \
                 name = ?2, description = ?3, system_prompt = ?4, framework = ?5, \
                 llm = ?6, allowed_models = ?7, reasoning_effort = ?8, updated_at = ?9 \
                 WHERE id = ?1 AND builtin = 0",
                libsql::params![
                    id.as_str().to_string(),
                    update.name.clone(),
                    update.description.clone(),
                    update.system_prompt.clone(),
                    update.framework.as_str(),
                    update.llm.as_ref().map(|l| l.as_str().to_string()),
                    allowed_models,
                    update.reasoning_effort.map(|e| e.as_str().to_owned()),
                    super::time::now_us(),
                ],
            )
            .await
            .map_err(|e| name_conflict_err("update agent profile", &update.name, e))?;
        Ok(affected > 0)
    }

    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "UPDATE agent_profiles SET avatar_blob_id = ?2, updated_at = ?3 WHERE id = ?1",
                libsql::params![
                    id.as_str().to_string(),
                    blob_id.map(str::to_string),
                    super::time::now_us(),
                ],
            )
            .await
            .map_err(|e| col_err("set agent profile avatar", e))?;
        Ok(affected > 0)
    }

    async fn delete(&self, id: &AgentProfileId) -> Result<bool> {
        let conn = self.pool.conn();
        let affected = conn
            .execute(
                "DELETE FROM agent_profiles WHERE id = ?1 AND builtin = 0",
                libsql::params![id.as_str().to_string()],
            )
            .await
            .map_err(|e| col_err("delete agent profile", e))?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    async fn open_store() -> LibsqlAgentProfileStore {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        LibsqlAgentProfileStore::open(pool).await.unwrap()
    }

    fn now_us_precision() -> DateTime<Utc> {
        crate::libsql::time::from_us(crate::libsql::time::now_us()).unwrap()
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
            allowed_models: Vec::new(),
            reasoning_effort: None,
            builtin: false,
            created_at: now,
            updated_at: now,
        }
    }

    async fn raw_allowed_models(
        store: &LibsqlAgentProfileStore,
        id: &AgentProfileId,
    ) -> Option<String> {
        let mut rows = store
            .pool
            .conn()
            .query(
                "SELECT allowed_models FROM agent_profiles WHERE id = ?1",
                libsql::params![id.as_str().to_string()],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("row exists");
        row.get::<Option<String>>(0).unwrap()
    }

    fn content_update(name: &str) -> AgentProfileUpdate {
        AgentProfileUpdate {
            name: name.to_owned(),
            description: String::new(),
            system_prompt: None,
            framework: AgentFramework::Baybo,
            llm: None,
            allowed_models: Vec::new(),
            reasoning_effort: None,
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlAgentProfileStore::open(pool.clone()).await.unwrap();
        let builtin = AgentProfileId::builtin();
        assert!(
            store
                .set_avatar(&builtin, Some("sha256:aa.bb"))
                .await
                .unwrap()
        );

        let store = LibsqlAgentProfileStore::open(pool).await.unwrap();
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

    #[tokio::test]
    async fn allowed_models_and_effort_round_trip() {
        let store = open_store().await;
        let mut row = custom_row("Tuned");
        row.allowed_models = vec![LlmEntryName::from("fast"), LlmEntryName::from("deep")];
        row.reasoning_effort = Some(baybo_model::ReasoningEffort::High);
        store.create(&row).await.unwrap();

        let back = store.get(&row.id).await.unwrap().unwrap();
        assert_eq!(back.allowed_models, row.allowed_models, "order preserved");
        assert_eq!(
            back.reasoning_effort,
            Some(baybo_model::ReasoningEffort::High)
        );

        // Full replace resets both to inherit/unrestricted.
        assert!(
            store
                .update(&row.id, &content_update("Tuned 2"))
                .await
                .unwrap()
        );
        let reset = store.get(&row.id).await.unwrap().unwrap();
        assert!(reset.allowed_models.is_empty());
        assert!(reset.reasoning_effort.is_none());
        assert_eq!(
            raw_allowed_models(&store, &row.id).await,
            None,
            "full-replace reset must store SQL NULL, not \"[]\""
        );

        // Empty set stores NULL (round-trips as empty, not "[]").
        let plain = custom_row("Plain");
        store.create(&plain).await.unwrap();
        let plain_back = store.get(&plain.id).await.unwrap().unwrap();
        assert!(plain_back.allowed_models.is_empty());
        assert_eq!(
            raw_allowed_models(&store, &plain.id).await,
            None,
            "empty-set create must store SQL NULL, not \"[]\""
        );
    }

    #[tokio::test]
    async fn corrupt_effort_or_set_column_errors_on_read() {
        let store = open_store().await;
        let row = custom_row("Broken");
        store.create(&row).await.unwrap();
        store
            .pool
            .conn()
            .execute(
                "UPDATE agent_profiles SET reasoning_effort = 'ultra' WHERE id = ?1",
                libsql::params![row.id.as_str().to_string()],
            )
            .await
            .unwrap();
        assert!(
            store.get(&row.id).await.is_err(),
            "unknown effort must error"
        );
        store
            .pool
            .conn()
            .execute(
                "UPDATE agent_profiles SET reasoning_effort = NULL, allowed_models = 'not-json' WHERE id = ?1",
                libsql::params![row.id.as_str().to_string()],
            )
            .await
            .unwrap();
        assert!(
            store.get(&row.id).await.is_err(),
            "malformed set must error"
        );
    }
}
