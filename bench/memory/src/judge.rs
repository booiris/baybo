//! LLM-as-judge: a single completion grading a candidate answer against the
//! gold answer. Every graded question is answerable — LOCOMO's adversarial
//! category is dropped before QA (see `run`), so there is no abstention path.

use anyhow::Result;

use crate::llm::ChatClient;

#[derive(Debug, Clone)]
pub struct Verdict {
    pub correct: bool,
    pub reason: String,
}

// Generosity + temporal-leniency clauses mirror OpenViking's openclaw LoCoMo
// judge (benchmark/locomo/openclaw/judge.py) so scores track upstream. Every
// graded question is answerable: LOCOMO's adversarial category (unanswerable
// distractors with no gradeable gold) is dropped before QA, as upstream does.
const JUDGE_SYSTEM: &str = r#"You grade a memory-augmented QA system against reference answers drawn from a long multi-session conversation.
Reply with exactly CORRECT or INCORRECT on the first line, then a one-sentence reason.

Be generous: a candidate is CORRECT if it conveys the same factual content as the reference — even if phrased differently, much longer, or with extra detail — as long as it touches on the same topic as the reference answer. For example, for the reference "a shell necklace", a candidate mentioning a shell necklace amid other detail is CORRECT.

For time-related questions the reference is a specific date/month/year. Be generous here too: a candidate is CORRECT as long as it refers to the same date or time period, even if the format differs (e.g. "May 7th" vs "7 May") or it uses a relative reference (e.g. "last Tuesday"). Mark INCORRECT only when it names a different date or period."#;

fn user_prompt(question: &str, gold: &str, candidate: &str) -> String {
    format!(
        "Question: {question}\nReference answer: {gold}\nCandidate answer: {candidate}\n\nIs the candidate CORRECT or INCORRECT?"
    )
}

/// Grade one answer with the judge model.
pub async fn judge(
    judge_llm: &ChatClient,
    question: &str,
    gold: &str,
    candidate: &str,
) -> Result<Verdict> {
    let reply = judge_llm
        .chat(JUDGE_SYSTEM, &user_prompt(question, gold, candidate))
        .await?;
    Ok(parse_verdict(&reply))
}

/// Parse the judge's reply. Checks INCORRECT first because it contains CORRECT
/// as a substring; an unparseable reply defaults to incorrect.
fn parse_verdict(text: &str) -> Verdict {
    let upper = text.to_uppercase();
    let correct = if upper.contains("INCORRECT") {
        false
    } else {
        upper.contains("CORRECT")
    };
    Verdict {
        correct,
        reason: text.trim().chars().take(280).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verdict_lines() {
        assert!(parse_verdict("CORRECT\nThe answer matches.").correct);
        assert!(!parse_verdict("INCORRECT — wrong pet.").correct);
        assert!(!parse_verdict("hmm, unclear").correct);
        // "INCORRECT" must not be read as CORRECT via substring.
        assert!(!parse_verdict("Verdict: INCORRECT").correct);
    }
}
