use baybo_model::ProjectId;
use baybo_store::StorageError;

pub type Result<T> = std::result::Result<T, ProjectError>;

/// What can go wrong managing a project or the issues on its board.
///
/// The variants are the distinctions the gateway needs to answer with
/// different status codes; anything finer is a `reason` string.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("no such project: {0}")]
    NoSuchProject(ProjectId),

    #[error("project {project} has no issue #{number}")]
    NoSuchIssue { project: ProjectId, number: i64 },

    /// The request was well-formed but its content is not acceptable —
    /// an empty name, an over-long one, a workdir that overlaps the
    /// workspace.
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },

    /// Something with that name already exists.
    #[error("{0}")]
    Conflict(String),

    /// The project is archived: it can still be read, but not written to.
    #[error("project {0} is archived")]
    Archived(ProjectId),

    /// The backend failed. **Not** where a unique-index trip lands — see
    /// the `From` impl below.
    #[error(transparent)]
    Storage(StorageError),

    /// Materialising the project's working directory failed. Distinct from
    /// [`Self::Storage`] because the two fail for unrelated reasons, and a
    /// filesystem failure means the row was never written.
    #[error("project workdir: {0}")]
    Workdir(#[from] anyhow::Error),
}

impl ProjectError {
    pub fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            reason: reason.into(),
        }
    }
}

/// A unique-index trip is an ordinary, expected answer in this domain, so
/// it becomes [`ProjectError::Conflict`] rather than an opaque backend
/// failure. Discriminated here rather than at each call site, and rather
/// than in the HTTP layer: a caller that is not HTTP deserves the same
/// distinction, and a `#[from]` that flattened both into one variant is
/// exactly how a 409 becomes a 500.
impl From<StorageError> for ProjectError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Conflict(reason) => Self::Conflict(reason),
            other => Self::Storage(other),
        }
    }
}
