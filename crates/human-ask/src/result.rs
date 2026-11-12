//! The `AskUser` tool result a question ask resolves to — one home for the
//! envelope, so a settlement appends exactly what the live tool would have.

use baybo_model::human_ask::QuestionAnswer;
use serde_json::{Value, json};

const DECLINED_NOTE: &str =
    r#"the user declined to answer; proceed on your best judgment or ask in plain text"#;
const ABANDONED_REASON: &str = r#"user moved on without answering"#;

/// `{"answers": [...]}`; a question with neither a selection nor free text
/// is marked `"skipped": true`.
pub fn answered(answers: &[QuestionAnswer]) -> Value {
    let answers: Vec<Value> = answers
        .iter()
        .map(|answer| {
            let mut entry = json!({
                "header": answer.header,
                "selected": answer.selected,
                "other": answer.other,
            });
            if answer.selected.is_empty() && answer.other.is_none() {
                entry["skipped"] = Value::Bool(true);
            }
            entry
        })
        .collect();
    json!({ "answers": answers })
}

/// The user declined the questions as a whole.
pub fn declined() -> Value {
    json!({ "declined": true, "note": DECLINED_NOTE })
}

/// Nobody answered, and the conversation moved on.
pub fn abandoned() -> Value {
    json!({ "declined": true, "reason": ABANDONED_REASON })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unanswered_question_reads_skipped() {
        let value = answered(&[
            QuestionAnswer {
                header: "Auth".into(),
                selected: vec!["JWT".into()],
                other: None,
            },
            QuestionAnswer {
                header: "DB".into(),
                selected: Vec::new(),
                other: None,
            },
        ]);
        assert_eq!(value["answers"][0]["selected"][0], "JWT");
        assert!(value["answers"][0].get("skipped").is_none());
        assert_eq!(value["answers"][1]["skipped"], true);
    }

    #[test]
    fn declined_and_abandoned_both_read_declined() {
        assert_eq!(declined()["declined"], true);
        assert_eq!(abandoned()["declined"], true);
        assert_eq!(abandoned()["reason"], ABANDONED_REASON);
    }
}
