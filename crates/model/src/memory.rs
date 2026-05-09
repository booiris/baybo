use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Semantic category for a `MemoryEntry`. The four-variant shape mirrors
/// the design in `docs/modules/self-improvement.md`:
///
/// - `User` — facts and self-described preferences about the person
///   (role, expertise, working style, stack, project context that's
///   really about *them*, not the project).
/// - `Feedback` — corrections and validations the user has given the
///   agent ("don't do X"; "yes that approach was right"). Body should
///   carry a `Why:` line and a `How to apply:` line.
/// - `Project` — in-flight work / decisions / deadlines specific to the
///   current project. Body should carry `Why:` and `How to apply:` lines.
/// - `Reference` — pointers to where information lives in external
///   systems (Linear, Slack, dashboards, etc.).
///
/// Migration: pre-self_improvement rows used `UserPreference` / `KeyFact`.
/// Both deserialize to `User` via serde aliases so old rows keep loading
/// without an in-place SQL rewrite; the next time the entry is rewritten
/// it gets the new tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum MemoryCategory {
    #[serde(alias = "UserPreference", alias = "KeyFact")]
    User,
    Feedback,
    Project,
    Reference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub user_id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub importance: f32,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub source_session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl MemoryEntry {
    pub fn new(
        user_id: String,
        content: String,
        category: MemoryCategory,
        importance: f32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            user_id,
            content,
            category,
            importance: importance.clamp(0.0, 1.0),
            embedding: None,
            created_at: now,
            last_accessed: now,
            source_session_id: None,
            expires_at: None,
        }
    }
}
