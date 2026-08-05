//! Project and issue lifecycle: validation, workdir materialisation, and
//! the board's write surface.
//!
//! The store below is a dumb writer. Everything that has to be true before
//! a row lands — a name that isn't blank, a workdir that doesn't overlap
//! baybo's own workspace, an issue that names a project that exists — is
//! decided here, once, so the HTTP layer and any future tool caller get
//! the same answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{AgentFramework, AgentProfileId, IssueId, MAX_PROJECT_NAME_CHARS, ProjectId};
use baybo_store::AgentProfileStore;
use baybo_store::project::{
    IssuePriority, IssueRow, IssueStatus, IssueUpdate, NewIssue, ProjectRow, ProjectStore,
    ProjectUpdate,
};
use baybo_workspace::WorkspacePaths;

use crate::error::{ProjectError, Result};

/// Upper bound on an issue title (chars, after trim). Long enough for a
/// sentence, short enough that a card face can show it.
pub const MAX_ISSUE_TITLE_CHARS: usize = 200;

/// What a caller supplies to open a project.
#[derive(Debug, Clone, Default)]
pub struct NewProject {
    pub name: String,
    pub description: String,
    /// Absolute path to an existing git repository. `None` means "make me
    /// one": the manager materialises `work/<slug>` and initialises it, so
    /// starting a project never requires having a repo first.
    pub workdir: Option<String>,
}

/// What a caller supplies to open an issue. Status is where the card
/// lands; there is no separate "column" concept.
#[derive(Debug, Clone)]
pub struct NewIssueRequest {
    pub title: String,
    pub description: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub assignee: Option<AgentProfileId>,
}

pub struct ProjectManager {
    store: Arc<dyn ProjectStore>,
    agents: Arc<dyn AgentProfileStore>,
    paths: WorkspacePaths,
}

impl ProjectManager {
    pub fn new(
        store: Arc<dyn ProjectStore>,
        agents: Arc<dyn AgentProfileStore>,
        paths: WorkspacePaths,
    ) -> Self {
        Self {
            store,
            agents,
            paths,
        }
    }

    pub async fn create_project(&self, new: NewProject) -> Result<ProjectRow> {
        let name = validate_name(&new.name)?;
        let id = ProjectId::generate();

        // Filesystem first, row second. A crash between the two leaves an
        // empty directory nobody references — inert. The other order leaves
        // a project pointing at somewhere that does not exist, which every
        // later run would have to handle.
        let workdir = match new.workdir.as_deref().map(str::trim) {
            Some(given) if !given.is_empty() => {
                validate_workdir(given, &self.paths)?;
                let path = Path::new(given);
                if !path.join(".git").exists() {
                    return Err(ProjectError::invalid(
                        "workdir",
                        format!(
                            "{given} is not a git repository. Leave the field empty to have \
                             one created under the workspace instead."
                        ),
                    ));
                }
                given.to_owned()
            }
            _ => self.materialise_workdir(&name).await?,
        };

        let now = chrono::Utc::now();
        let row = ProjectRow {
            id,
            name,
            description: new.description.trim().to_owned(),
            workdir,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.store.create_project(&row).await?;
        Ok(row)
    }

    /// Create `work/<slug>/` and make it a git repository.
    ///
    /// An existing non-empty directory is an error rather than a silent
    /// reuse: `work/` is shared with the bash tool's scratch space, and
    /// adopting whatever is already sitting there would hand a project a
    /// working tree it did not create.
    async fn materialise_workdir(&self, name: &str) -> Result<String> {
        let slug = slugify(name);
        let dir = self.paths.work_dir().join(&slug);
        if dir.exists() {
            let mut entries = tokio::fs::read_dir(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?;
            let occupied = entries
                .next_entry()
                .await
                .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?
                .is_some();
            if occupied {
                return Err(ProjectError::invalid(
                    "workdir",
                    format!(
                        "{} already exists and is not empty. Point the project at it \
                         explicitly, or rename the project.",
                        dir.display()
                    ),
                ));
            }
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;
        git_init(&dir).await?;
        Ok(dir.to_string_lossy().into_owned())
    }

    pub async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>> {
        Ok(self.store.list_projects(include_archived).await?)
    }

    pub async fn get_project(&self, id: &ProjectId) -> Result<ProjectRow> {
        self.store
            .get_project(id)
            .await?
            .ok_or_else(|| ProjectError::NoSuchProject(id.clone()))
    }

    pub async fn update_project(
        &self,
        id: &ProjectId,
        update: ProjectUpdate,
    ) -> Result<ProjectRow> {
        let existing = self.get_project(id).await?;
        if existing.archived_at.is_some() {
            return Err(ProjectError::Archived(id.clone()));
        }
        let update = ProjectUpdate {
            name: validate_name(&update.name)?,
            description: update.description.trim().to_owned(),
        };
        self.store.update_project(id, &update).await?;
        self.get_project(id).await
    }

    /// Archive or restore. Archiving is always allowed — including on an
    /// already-archived project, which is how a restore is idempotent.
    pub async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<ProjectRow> {
        if !self.store.set_project_archived(id, archived).await? {
            return Err(ProjectError::NoSuchProject(id.clone()));
        }
        self.get_project(id).await
    }

    pub async fn list_issues(&self, project: &ProjectId) -> Result<Vec<IssueRow>> {
        // Resolve the parent first: an unknown board and an empty board are
        // different answers, and only this call can tell them apart.
        self.get_project(project).await?;
        Ok(self.store.list_issues(project).await?)
    }

    pub async fn get_issue(&self, project: &ProjectId, number: i64) -> Result<IssueRow> {
        self.get_project(project).await?;
        self.store
            .get_issue(project, number)
            .await?
            .ok_or_else(|| ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            })
    }

    pub async fn create_issue(
        &self,
        project: &ProjectId,
        new: NewIssueRequest,
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        let title = validate_issue_title(&new.title)?;
        if let Some(assignee) = new.assignee.as_ref() {
            self.validate_assignee(assignee).await?;
        }
        self.validate_staffing(new.status, new.assignee.as_ref())?;
        Ok(self
            .store
            .create_issue(&NewIssue {
                id: IssueId::generate(),
                project_id: project.clone(),
                title,
                description: new.description.trim().to_owned(),
                status: new.status,
                priority: new.priority,
                assignee: new.assignee,
                created_at: chrono::Utc::now(),
            })
            .await?)
    }

    pub async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        update: IssueUpdate,
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        if update.is_empty() {
            return Err(ProjectError::invalid("update", "sets no field"));
        }
        if let Some(next) = update.assignee.as_ref() {
            if let Some(assignee) = next.as_ref() {
                self.validate_assignee(assignee).await?;
            }
            // Checked against the column the issue is actually in: dropping
            // the assignee of in-flight work recreates exactly the zombie
            // the staffing rule exists to prevent.
            let current = self.get_issue(project, number).await?;
            self.validate_staffing(current.status, next.as_ref())?;
        }
        let update = IssueUpdate {
            title: update
                .title
                .as_deref()
                .map(validate_issue_title)
                .transpose()?,
            description: update.description.map(|d| d.trim().to_owned()),
            blocked_reason: update.blocked_reason.map(|reason| {
                reason.and_then(|r| {
                    let trimmed = r.trim();
                    // An all-whitespace reason is an unblock, not a block
                    // with a blank explanation.
                    (!trimmed.is_empty()).then(|| trimmed.to_owned())
                })
            }),
            ..update
        };
        if !self.store.update_issue(project, number, &update).await? {
            return Err(ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            });
        }
        self.get_issue(project, number).await
    }

    /// Move an issue into `status`, with `ordered_numbers` giving that
    /// column's full contents in their new order.
    pub async fn move_issue(
        &self,
        project: &ProjectId,
        number: i64,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        if !ordered_numbers.contains(&number) {
            return Err(ProjectError::invalid(
                "ordered_numbers",
                "must contain the moved issue — it is the destination column's new contents",
            ));
        }
        let issue = self.get_issue(project, number).await?;
        self.validate_staffing(status, issue.assignee.as_ref())?;
        if !self
            .store
            .move_issue(project, number, status, ordered_numbers)
            .await?
        {
            return Err(ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            });
        }
        self.get_issue(project, number).await
    }

    /// An assignee has to be an agent that exists and can actually run.
    ///
    /// External claude/codex profiles are refused: a top-level session
    /// cannot be bound to a non-baybo framework today — the external
    /// backend exists only inside the subagent spawner — so assigning one
    /// would produce a card that can never start.
    async fn validate_assignee(&self, assignee: &AgentProfileId) -> Result<()> {
        let profile = self
            .agents
            .get(assignee)
            .await?
            .ok_or_else(|| ProjectError::invalid("assignee", format!("no agent {assignee}")))?;
        if profile.framework != AgentFramework::Baybo {
            return Err(ProjectError::invalid(
                "assignee",
                format!(
                    "{assignee} runs on {}, which cannot yet host an issue's session — \
                     assign a baybo agent",
                    profile.framework.as_str()
                ),
            ));
        }
        Ok(())
    }

    /// In Progress means somebody is on it. A card in that column with no
    /// assignee is work the board claims is happening and nobody is doing;
    /// every other column is free to be unassigned.
    fn validate_staffing(
        &self,
        status: IssueStatus,
        assignee: Option<&AgentProfileId>,
    ) -> Result<()> {
        if status == IssueStatus::InProgress && assignee.is_none() {
            return Err(ProjectError::invalid(
                "assignee",
                "In Progress needs an assignee — assign someone before starting the work",
            ));
        }
        Ok(())
    }

    /// Resolve a project and refuse if it is archived. Every write path
    /// starts here, so "archived is read-only" is one rule in one place
    /// rather than a check each endpoint has to remember.
    async fn writable_project(&self, id: &ProjectId) -> Result<ProjectRow> {
        let project = self.get_project(id).await?;
        if project.archived_at.is_some() {
            return Err(ProjectError::Archived(id.clone()));
        }
        Ok(project)
    }
}

fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid("name", "must not be empty"));
    }
    if trimmed.chars().count() > MAX_PROJECT_NAME_CHARS {
        return Err(ProjectError::invalid(
            "name",
            format!("longer than {MAX_PROJECT_NAME_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

fn validate_issue_title(title: &str) -> Result<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid("title", "must not be empty"));
    }
    if trimmed.chars().count() > MAX_ISSUE_TITLE_CHARS {
        return Err(ProjectError::invalid(
            "title",
            format!("longer than {MAX_ISSUE_TITLE_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// A project name reduced to a directory name: lowercase, ASCII
/// alphanumerics and dashes, no runs, no leading or trailing dash.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        // A name of pure punctuation still needs somewhere to live.
        format!("project-{}", ProjectId::generate().as_str().to_lowercase())
    } else {
        slug
    }
}

async fn git_init(dir: &PathBuf) -> Result<()> {
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(dir)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `git init {}`: {e}", dir.display()))?;
    if !status.success() {
        return Err(ProjectError::Workdir(anyhow::anyhow!(
            "`git init {}` exited with {status}",
            dir.display()
        )));
    }
    Ok(())
}

/// Refuse a workdir that overlaps baybo's own workspace.
///
/// A project's checkout is bound read-write into every shell command its
/// team runs, so it must be neither inside the workspace tree nor a parent
/// of it.
pub fn validate_workdir(workdir: &str, paths: &WorkspacePaths) -> Result<()> {
    let path = Path::new(workdir);
    if !path.is_absolute() {
        return Err(ProjectError::invalid("workdir", "must be an absolute path"));
    }
    // Canonicalise before comparing, because the sandbox resolves symlinks
    // when it mounts: a lexical-only check passes a link aimed at `state/`
    // and then binds exactly what it refused. A path that does not exist
    // yet can only be checked lexically — and cannot be bound either.
    let resolve = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let target = resolve(path);
    let root = resolve(&baybo_workspace::absolutise(paths.root()));
    let work = resolve(&baybo_workspace::absolutise(&paths.work_dir()));
    // Both directions. A descendant is the obvious one; an **ancestor** is
    // the one that bites, because the default workspace lives in `~/.baybo`
    // and `workdir = $HOME` would bind the whole home directory — baybo's
    // own state, keys and all — read-write into every shell the team runs.
    // `work/` is the single exemption: a shell may already write there.
    let inside = target.starts_with(&root) && !target.starts_with(&work);
    // No `work/` exemption on this side: containing `work/` does not stop a
    // parent from containing `state/` and `.key/` as well.
    let swallows = root.starts_with(&target);
    if inside || swallows {
        return Err(ProjectError::invalid(
            "workdir",
            format!(
                "{} overlaps baybo's own workspace at {}. A project's checkout is \
                 bound read-write into every shell command its team runs, so it must \
                 be neither inside that tree nor a parent of it (a checkout under {} \
                 is fine).",
                target.display(),
                root.display(),
                work.display(),
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_directory_safe() {
        assert_eq!(slugify("baybo"), "baybo");
        assert_eq!(slugify("My Project!"), "my-project");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("../escape"), "escape");
        assert_eq!(slugify("a//b"), "a-b");
        assert!(
            slugify("!!!").starts_with("project-"),
            "a name with nothing usable still gets a directory"
        );
    }
}
