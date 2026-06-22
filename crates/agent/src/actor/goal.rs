//! Pure `/goal` command parsing and the user-facing lifecycle notice text. The
//! actor ([`super`]) owns the continuation engine and drives these; keeping the
//! stateless parse/render layer here keeps that engine readable.

use aura_channels::{
    GOAL_BUDGET_FLAG, GOAL_CLEAR_SUBCOMMAND, GOAL_PAUSE_SUBCOMMAND, GOAL_RESUME_SUBCOMMAND,
};
use aura_model::Goal;

/// Parsed form of a `/goal` command (everything after the leading `/goal`).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalCommand {
    View,
    Pause,
    Resume,
    Clear,
    Set {
        objective: String,
        budget: Option<u64>,
    },
    Error(String),
}

/// Parse the joined `/goal ...` slash text. Tolerant of casing and a Telegram
/// `@bot` suffix on the command token (both stripped by taking everything after
/// the first whitespace).
pub(super) fn parse_goal_command(command_text: &str) -> GoalCommand {
    let rest = command_text
        .trim()
        .strip_prefix('/')
        .and_then(|s| s.split_once(char::is_whitespace).map(|(_, r)| r))
        .unwrap_or("")
        .trim();
    if rest.is_empty() {
        return GoalCommand::View;
    }
    // Subcommands match case-insensitively (the command token itself already
    // does), so `/Goal Pause` stops the goal rather than overwriting it with a
    // new objective "Pause". Only the bare subcommand word is normalized; the
    // objective (the `Set` fallback below) keeps its original casing.
    match rest.to_ascii_lowercase().as_str() {
        GOAL_PAUSE_SUBCOMMAND => return GoalCommand::Pause,
        GOAL_RESUME_SUBCOMMAND => return GoalCommand::Resume,
        GOAL_CLEAR_SUBCOMMAND => return GoalCommand::Clear,
        _ => {}
    }
    let budget_eq = format!("{GOAL_BUDGET_FLAG}=");
    let mut budget: Option<u64> = None;
    let mut objective_tokens: Vec<&str> = Vec::new();
    let mut iter = rest.split_whitespace();
    while let Some(token) = iter.next() {
        let raw_value = if token == GOAL_BUDGET_FLAG {
            Some(iter.next())
        } else {
            token.strip_prefix(budget_eq.as_str()).map(Some)
        };
        match raw_value {
            Some(Some(value)) => match value.replace('_', "").parse::<u64>() {
                Ok(n) if n > 0 => budget = Some(n),
                _ => {
                    return GoalCommand::Error(format!(
                        "Invalid `--budget` value {value:?} — it must be a positive integer."
                    ));
                }
            },
            Some(None) => {
                return GoalCommand::Error(
                    "`--budget` needs a number, e.g. `/goal <objective> --budget 50000`.".into(),
                );
            }
            None => objective_tokens.push(token),
        }
    }
    let objective = objective_tokens.join(" ");
    if objective.trim().is_empty() {
        return GoalCommand::Error(
            "Provide an objective, e.g. `/goal keep the tests green --budget 50000`.".into(),
        );
    }
    GoalCommand::Set { objective, budget }
}

/// Human-readable `Hh Mm` / `Mm Ss` / `Ss` duration for goal usage display.
fn fmt_goal_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Appended to a goal-edit confirmation so the user sees the cap actually changed.
pub(super) fn budget_note(budget: Option<u64>) -> String {
    budget
        .map(|b| format!(" (token budget: {b})"))
        .unwrap_or_default()
}

pub(super) fn goal_set_text(goal: &Goal) -> String {
    format!(
        "🎯 Goal set{}: \"{}\"\nI'll keep working toward it autonomously, turn after turn, until it's done. Send `/goal` to check progress, or `/goal pause` to stop.",
        budget_note(goal.token_budget),
        goal.objective
    )
}

pub(super) fn goal_status_text(goal: &Goal) -> String {
    let mut s = format!(
        "🎯 Goal ({}): \"{}\"\n",
        goal.status.as_str(),
        goal.objective
    );
    match goal.token_budget {
        Some(b) => s.push_str(&format!(
            "Tokens: {} / {} ({} remaining)\n",
            goal.tokens_used,
            b,
            goal.remaining_budget().unwrap_or(0)
        )),
        None => s.push_str(&format!("Tokens used: {}\n", goal.tokens_used)),
    }
    s.push_str(&format!(
        "Time: {}",
        fmt_goal_duration(goal.time_used_seconds)
    ));
    s
}

pub(super) fn goal_complete_text(goal: &Goal) -> String {
    format!(
        "✅ Goal complete: \"{}\" — {} tokens used over {}.",
        goal.objective,
        goal.tokens_used,
        fmt_goal_duration(goal.time_used_seconds)
    )
}

pub(super) fn goal_blocked_text(goal: &Goal) -> String {
    format!(
        "⛔ Goal blocked: \"{}\". The agent hit a recurring impasse it couldn't get past. Run `/goal resume` to retry, or `/goal clear` to drop it.",
        goal.objective
    )
}

pub(super) fn goal_budget_reached_text(goal: &Goal) -> String {
    let budget = goal
        .token_budget
        .map(|b| b.to_string())
        .unwrap_or_else(|| "—".to_string());
    format!(
        "⏳ Goal token budget reached ({} / {}). I wrapped up; raise the budget and run `/goal resume` to continue.",
        goal.tokens_used, budget
    )
}

pub(super) fn goal_spend_capped_text() -> String {
    "🛑 Goal stopped: the global spend limit was reached, so the next turn could not run. Run `/goal resume` once the limit resets.".to_string()
}

pub(super) fn goal_paused_text() -> &'static str {
    "⏸️ Goal paused. The autonomous loop is stopped; run `/goal resume` to continue."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_goal_subcommands_are_case_insensitive() {
        assert!(matches!(parse_goal_command("/goal"), GoalCommand::View));
        assert!(matches!(
            parse_goal_command("/goal pause"),
            GoalCommand::Pause
        ));
        // `/Goal Pause` must pause, NOT create a goal named "Pause".
        assert!(matches!(
            parse_goal_command("/Goal Pause"),
            GoalCommand::Pause
        ));
        assert!(matches!(
            parse_goal_command("/goal@Bot RESUME"),
            GoalCommand::Resume
        ));
        assert!(matches!(
            parse_goal_command("/goal Clear"),
            GoalCommand::Clear
        ));
    }

    #[test]
    fn parse_goal_set_preserves_objective_casing_and_budget() {
        match parse_goal_command("/goal Ship The Feature --budget 5000") {
            GoalCommand::Set { objective, budget } => {
                assert_eq!(objective, "Ship The Feature");
                assert_eq!(budget, Some(5000));
            }
            other => panic!("expected Set, got {other:?}"),
        }
        // `--budget=N` form, objective casing intact, no accidental subcommand.
        match parse_goal_command("/goal Pause The World --budget=10_000") {
            GoalCommand::Set { objective, budget } => {
                assert_eq!(objective, "Pause The World");
                assert_eq!(budget, Some(10_000));
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }
}
