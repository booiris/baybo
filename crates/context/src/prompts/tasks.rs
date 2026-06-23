//! Framing for the per-turn planning-checklist reminder (`Task*`).
//!
//! Rendered from the durable `session_tasks` list when the agent loop injects
//! the (throttled) reminder — it fetches the list and hands it to
//! [`crate::ContextManager::refresh_task_reminder`]. The reminder rides as a
//! transient tail row that is never persisted, so the list the model sees
//! matches the store and the prompt-cache prefix stays stable. Mirrors Claude
//! Code's periodic todo nudge and Codex's `update_plan`.

use baybo_model::{Task, TaskStatus};

const TASK_LIST_HEADER: &str = r#"Your task list for this session (the user sees this as a live checklist). Keep it accurate with TaskCreate / TaskUpdate: mark a task in_progress when you start it (at most one at a time) and completed the instant it's done. If a task turns out too complex mid-execution, split it into finer sub-tasks with TaskCreate (link prerequisites via depends_on)."#;

fn status_marker(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "[ ]",
        TaskStatus::InProgress => "[~]",
        TaskStatus::Completed => "[x]",
    }
}

/// Render the checklist into a `<system-reminder>` block. Each line carries a
/// status marker, the task subject, its id (so the model can target it with
/// `TaskUpdate`), and any dependency ids. The `description` body is omitted to
/// keep the reminder scannable — the model can `TaskGet` for the full record.
///
/// Caller guarantees `tasks` is non-empty (an empty list clears the reminder
/// instead of rendering an empty block).
pub fn render_task_list(tasks: &[Task]) -> String {
    let mut out = String::from("<system-reminder>\n");
    out.push_str(TASK_LIST_HEADER);
    out.push_str("\n\n");
    for task in tasks {
        out.push_str(status_marker(task.status));
        out.push(' ');
        out.push_str(&task.subject);
        out.push_str(" (id=");
        out.push_str(&task.id.to_string());
        out.push(')');
        if !task.depends_on.is_empty() {
            let deps: Vec<String> = task.depends_on.iter().map(|d| d.to_string()).collect();
            out.push_str(" [after: ");
            out.push_str(&deps.join(", "));
            out.push(']');
        }
        out.push('\n');
    }
    out.push_str("</system-reminder>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::TaskId;
    use chrono::Utc;

    fn task(subject: &str, status: TaskStatus) -> Task {
        let now = Utc::now();
        Task {
            id: TaskId::new(),
            subject: subject.into(),
            description: "ignored in the reminder".into(),
            status,
            depends_on: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn renders_markers_ids_and_omits_description() {
        let tasks = vec![
            task("write the table", TaskStatus::Completed),
            task("wire the runtime", TaskStatus::InProgress),
            task("add tests", TaskStatus::Pending),
        ];
        let out = render_task_list(&tasks);
        assert!(out.starts_with("<system-reminder>"));
        assert!(out.ends_with("</system-reminder>"));
        assert!(out.contains("[x] write the table (id="));
        assert!(out.contains("[~] wire the runtime (id="));
        assert!(out.contains("[ ] add tests (id="));
        assert!(!out.contains("ignored in the reminder"));
    }

    #[test]
    fn renders_dependencies() {
        let mut a = task("first", TaskStatus::Completed);
        let mut b = task("second", TaskStatus::Pending);
        b.depends_on = vec![a.id];
        a.depends_on = Vec::new();
        let out = render_task_list(&[a.clone(), b]);
        assert!(out.contains(&format!("[after: {}]", a.id)));
    }
}
