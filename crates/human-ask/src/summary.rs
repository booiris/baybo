//! Derivation of [`AskStepSummary`] and display labels for `AskUser` calls.
//!
//! One home shared by the live path (`AgentLoop::emit_tool_completed`) and
//! REST transcript reconstruction (`baybo-gateway::api::admin::chat`), so a
//! live turn and a reloaded transcript render identical step summaries.

use baybo_model::human_ask::{AskStepQuestion, AskStepSummary};
use serde_json::Value;

/// First question's trimmed `header` from an `AskUser` call's input, used as
/// the tool's short progress/step label on both live and REST planes.
pub fn first_question_header(input: &Value) -> Option<String> {
    input
        .get("questions")?
        .as_array()?
        .first()?
        .get("header")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Derive a typed [`AskStepSummary`] from an `AskUser` call's `input` and
/// parsed JSON `result`. Returns `None` when the input does not parse as a
/// non-empty [`QuestionRequest`](baybo_model::human_ask::QuestionRequest) or
/// the result is not an `answers`, `declined` or `cancelled` envelope.
pub fn ask_step_summary(input: &Value, result: &Value) -> Option<AskStepSummary> {
    let req = crate::tools::validate_question_request(input.clone()).ok()?;

    let declined = result.get("declined").and_then(Value::as_bool) == Some(true);
    let cancelled = result.get("cancelled").and_then(Value::as_bool) == Some(true);
    if declined || cancelled {
        let questions = req
            .questions
            .into_iter()
            .map(|q| AskStepQuestion {
                header: q.header,
                question: q.question,
                options: q.options,
                multi_select: q.multi_select,
                selected: Vec::new(),
                other: None,
                skipped: false,
            })
            .collect();
        return Some(AskStepSummary {
            declined,
            cancelled,
            questions,
        });
    }

    let answers = result.get("answers")?.as_array()?;
    let questions = req
        .questions
        .into_iter()
        .map(|q| {
            let header = q.header;
            let entry = answers.iter().find(|a| {
                a.get("header")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|h| h == header)
            });
            let selected: Vec<String> = entry
                .and_then(|a| a.get("selected"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let other = entry
                .and_then(|a| a.get("other"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let skipped = entry
                .and_then(|a| a.get("skipped"))
                .and_then(Value::as_bool)
                .unwrap_or(selected.is_empty() && other.is_none());
            AskStepQuestion {
                header,
                question: q.question,
                options: q.options,
                multi_select: q.multi_select,
                selected,
                other,
                skipped,
            }
        })
        .collect();

    Some(AskStepSummary {
        declined: false,
        cancelled: false,
        questions,
    })
}

/// One-line human summary for plain-text surfaces and fallback step text:
/// e.g. `"Asked 2 questions · Auth: JWT · DB: Postgres, Redis"`.
pub fn format_ask_summary(summary: &AskStepSummary) -> String {
    let n = summary.questions.len();
    let lead = if n == 1 {
        "Asked 1 question".to_owned()
    } else {
        format!("Asked {n} questions")
    };
    if summary.cancelled {
        return format!("{lead} · cancelled");
    }
    if summary.declined {
        return format!("{lead} · declined");
    }
    let mut parts = vec![lead];
    for q in &summary.questions {
        if q.skipped {
            parts.push(format!("{}: skipped", q.header));
            continue;
        }
        let mut choices: Vec<&str> = q.selected.iter().map(String::as_str).collect();
        if let Some(other) = q.other.as_deref().filter(|s| !s.is_empty()) {
            choices.push(other);
        }
        if choices.is_empty() {
            parts.push(format!("{}: skipped", q.header));
        } else {
            parts.push(format!("{}: {}", q.header, choices.join(", ")));
        }
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use baybo_model::human_ask::QuestionAnswer;
    use serde_json::json;

    use super::*;
    use crate::result;

    fn sample_input() -> Value {
        json!({
            "questions": [
                {
                    "header": "Auth",
                    "question": "Which auth scheme?",
                    "options": [
                        { "label": "JWT", "description": "stateless" },
                        { "label": "Session", "description": "server-side" }
                    ],
                    "multi_select": false
                },
                {
                    "header": "DB",
                    "question": "Which stores?",
                    "options": [
                        { "label": "Postgres", "description": "relational" },
                        { "label": "Redis", "description": "cache" }
                    ],
                    "multi_select": true
                }
            ]
        })
    }

    #[test]
    fn first_header_pulls_first_question_header() {
        assert_eq!(
            first_question_header(&sample_input()).as_deref(),
            Some("Auth")
        );
        assert_eq!(first_question_header(&json!({})), None);
        assert_eq!(first_question_header(&json!({ "questions": [] })), None);
    }

    #[test]
    fn summarizes_answered_and_skipped_tabs_and_formats_line() {
        let res = result::answered(&[
            QuestionAnswer {
                header: "Auth".into(),
                selected: vec!["JWT".into()],
                other: None,
            },
            QuestionAnswer {
                header: "DB".into(),
                selected: vec!["Postgres".into(), "Redis".into()],
                other: None,
            },
        ]);
        let summary = ask_step_summary(&sample_input(), &res).expect("summary");
        assert!(!summary.declined);
        assert_eq!(summary.questions.len(), 2);
        assert_eq!(summary.questions[0].selected, vec!["JWT"]);
        assert!(!summary.questions[0].skipped);
        assert_eq!(summary.questions[1].selected, vec!["Postgres", "Redis"]);
        assert_eq!(
            format_ask_summary(&summary),
            "Asked 2 questions · Auth: JWT · DB: Postgres, Redis"
        );

        let partial = result::answered(&[
            QuestionAnswer {
                header: "Auth".into(),
                selected: Vec::new(),
                other: Some("OAuth2".into()),
            },
            QuestionAnswer {
                header: "DB".into(),
                selected: Vec::new(),
                other: None,
            },
        ]);
        let partial_summary = ask_step_summary(&sample_input(), &partial).expect("summary");
        assert_eq!(
            partial_summary.questions[0].other.as_deref(),
            Some("OAuth2")
        );
        assert!(!partial_summary.questions[0].skipped);
        assert!(partial_summary.questions[1].skipped);
        assert_eq!(
            format_ask_summary(&partial_summary),
            "Asked 2 questions · Auth: OAuth2 · DB: skipped"
        );
    }

    #[test]
    fn summarizes_declined_and_abandoned_preserving_questions() {
        for res in [result::declined(), result::abandoned()] {
            let summary = ask_step_summary(&sample_input(), &res).expect("summary");
            assert!(summary.declined);
            assert_eq!(summary.questions.len(), 2);
            assert_eq!(format_ask_summary(&summary), "Asked 2 questions · declined");
        }
    }

    #[test]
    fn cancellation_preserves_the_questions_without_claiming_an_answer() {
        let summary = ask_step_summary(&sample_input(), &result::cancelled()).expect("summary");
        assert!(summary.cancelled);
        assert!(!summary.declined);
        assert_eq!(
            format_ask_summary(&summary),
            "Asked 2 questions · cancelled"
        );
        assert!(
            summary
                .questions
                .iter()
                .all(|q| q.selected.is_empty() && q.other.is_none() && !q.skipped)
        );
    }

    #[test]
    fn ignores_non_ask_results_and_bad_inputs() {
        assert_eq!(ask_step_summary(&sample_input(), &json!("error")), None);
        assert_eq!(
            ask_step_summary(&json!({ "questions": [] }), &result::declined()),
            None
        );
    }
}
