//! The LOCOMO test set: parse the released JSON into the normalized
//! [`BenchSample`] IR. LOCOMO is a long, multi-session two-speaker dialogue with
//! categorized QA pairs (category 5 = adversarial: unanswerable distractors,
//! flagged here so the runner can drop them before scoring). Sessions live under
//! dynamic `session_{n}` / `session_{n}_date_time` keys. See the README for the
//! dataset source and full shape.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{BenchConversation, BenchQuestion, BenchSample, BenchSession, TestSet};

/// The LOCOMO "adversarial" category: the question asks about one speaker but
/// the fact belongs to the other, so its `adversarial_answer` is a distractor,
/// not a gradeable gold. The runner drops these before QA (upstream excludes the
/// category from scoring); the [`BenchQuestion::adversarial`] flag marks them.
const ADVERSARIAL_CATEGORY: u8 = 5;

pub struct Locomo;

impl TestSet for Locomo {
    fn name(&self) -> &'static str {
        "locomo"
    }

    fn load(&self, path: &Path) -> Result<Vec<BenchSample>> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read dataset {}", path.display()))?;
        let samples: Vec<Sample> = serde_json::from_str(&raw)
            .context("parse LOCOMO dataset (expected a JSON array of samples)")?;
        Ok(samples.iter().map(to_bench_sample).collect())
    }
}

/// Human label for a LOCOMO question category. Unknown numbers (the dataset is
/// research-released and could drift) fall back to `"unknown"` rather than
/// panicking, so a new category never breaks a run.
fn category_label(category: u8) -> &'static str {
    match category {
        1 => "multi-hop",
        2 => "temporal",
        3 => "open-domain",
        4 => "single-hop",
        ADVERSARIAL_CATEGORY => "adversarial",
        _ => "unknown",
    }
}

/// Map a raw LOCOMO sample into the harness IR: each turn becomes a
/// `"{speaker}: {text}"` string, each QA pair carries its resolved category
/// label and abstain flag.
fn to_bench_sample(sample: &Sample) -> BenchSample {
    let sessions = sample
        .conversation
        .sessions()
        .into_iter()
        .map(|s| BenchSession {
            index: s.index,
            date_time: s.date_time,
            turns: s
                .turns
                .iter()
                .map(|t| format!("{}: {}", t.speaker, t.content()))
                .collect(),
        })
        .collect();
    let questions = sample
        .qa
        .iter()
        .map(|qa| BenchQuestion {
            question: qa.question.clone(),
            gold_answer: qa.gold_answer(),
            adversarial: qa.is_adversarial(),
            category: category_label(qa.category).to_string(),
        })
        .collect();
    BenchSample {
        sample_id: sample.sample_id.clone(),
        conversation: BenchConversation { sessions },
        questions,
    }
}

// ---------------------------------------------------------------------------
// Raw LOCOMO JSON shapes (private; the rest of the crate sees only the IR)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Sample {
    #[serde(default)]
    sample_id: Option<String>,
    #[serde(default)]
    qa: Vec<Qa>,
    conversation: Conversation,
}

/// LOCOMO stores sessions under dynamic keys (`session_1`,
/// `session_1_date_time`, …), so the variable part is captured in `extra` and
/// re-assembled by [`Conversation::sessions`]. The `speaker_a` / `speaker_b`
/// header keys land in `extra` too and are ignored (each turn carries its own
/// speaker).
#[derive(Debug, Clone, Deserialize)]
struct Conversation {
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl Conversation {
    /// Re-assemble the `session_{n}` / `session_{n}_date_time` keys into ordered
    /// [`SessionView`]s. Sessions with no turns are dropped (a stray date key
    /// without a body never produces an empty session).
    fn sessions(&self) -> Vec<SessionView> {
        let mut by_index: BTreeMap<usize, SessionView> = BTreeMap::new();
        for (key, val) in &self.extra {
            let Some(rest) = key.strip_prefix("session_") else {
                continue;
            };
            if let Some(idx) = rest.strip_suffix("_date_time").and_then(|s| s.parse().ok()) {
                if let Some(date) = val.as_str() {
                    by_index
                        .entry(idx)
                        .or_insert_with(|| SessionView::empty(idx))
                        .date_time = Some(date.to_string());
                }
            } else if let Ok(idx) = rest.parse::<usize>() {
                let turns: Vec<Turn> = serde_json::from_value(val.clone()).unwrap_or_default();
                by_index
                    .entry(idx)
                    .or_insert_with(|| SessionView::empty(idx))
                    .turns = turns;
            }
        }
        by_index
            .into_values()
            .filter(|s| !s.turns.is_empty())
            .collect()
    }
}

struct SessionView {
    index: usize,
    date_time: Option<String>,
    turns: Vec<Turn>,
}

impl SessionView {
    fn empty(index: usize) -> Self {
        Self {
            index,
            date_time: None,
            turns: Vec::new(),
        }
    }
}

/// One utterance. Image-only turns carry `blip_caption` instead of `text`;
/// [`Turn::content`] prefers `text` and falls back to the caption.
#[derive(Debug, Clone, Deserialize)]
struct Turn {
    #[serde(default)]
    speaker: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    blip_caption: Option<String>,
}

impl Turn {
    fn content(&self) -> &str {
        if !self.text.is_empty() {
            &self.text
        } else {
            self.blip_caption.as_deref().unwrap_or("")
        }
    }
}

/// One QA pair. Adversarial (category 5) questions carry `adversarial_answer`
/// instead of `answer`; both are deserialized as raw JSON because LOCOMO answers
/// are sometimes numeric.
#[derive(Debug, Clone, Deserialize)]
struct Qa {
    question: String,
    #[serde(default)]
    answer: Option<Value>,
    #[serde(default)]
    adversarial_answer: Option<Value>,
    #[serde(default)]
    category: u8,
}

impl Qa {
    fn is_adversarial(&self) -> bool {
        self.category == ADVERSARIAL_CATEGORY
            || (self.answer.is_none() && self.adversarial_answer.is_some())
    }

    fn gold_answer(&self) -> String {
        match self.answer.as_ref().or(self.adversarial_answer.as_ref()) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
    [
      {
        "sample_id": "conv-1",
        "qa": [
          {"question": "What pet did Caroline adopt?", "answer": "A puppy named Biscuit", "category": 4, "evidence": ["D1:1"]},
          {"question": "How many puppies?", "answer": 1, "category": 2},
          {"question": "Where does Caroline bank?", "adversarial_answer": "Not mentioned", "category": 5}
        ],
        "conversation": {
          "speaker_a": "Caroline",
          "speaker_b": "Melanie",
          "session_1_date_time": "1:56 pm on 8 May, 2023",
          "session_1": [
            {"speaker": "Caroline", "dia_id": "D1:1", "text": "I just adopted a puppy named Biscuit!"},
            {"speaker": "Melanie", "dia_id": "D1:2", "text": "Aww, I'm allergic to dogs though."}
          ],
          "session_2_date_time": "9:00 am on 15 June, 2023",
          "session_2": [
            {"speaker": "Melanie", "dia_id": "D2:1", "text": "Started a job at a vet clinic!"},
            {"speaker": "Caroline", "dia_id": "D2:2", "blip_caption": "a photo of a clinic"},
            {"speaker": "Melanie", "dia_id": "D2:3", "text": "It's great."}
          ]
        }
      }
    ]
    "#;

    fn sample() -> Sample {
        let mut s: Vec<Sample> = serde_json::from_str(FIXTURE).expect("fixture parses");
        s.pop().expect("one sample")
    }

    #[test]
    fn sessions_parse_in_order_with_dates() {
        let sessions = sample().conversation.sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].index, 1);
        assert_eq!(
            sessions[0].date_time.as_deref(),
            Some("1:56 pm on 8 May, 2023")
        );
        assert_eq!(sessions[1].index, 2);
        assert_eq!(sessions[1].turns.len(), 3);
    }

    #[test]
    fn image_only_turn_falls_back_to_caption() {
        let sessions = sample().conversation.sessions();
        let img_turn = &sessions[1].turns[1];
        assert_eq!(img_turn.text, "");
        assert_eq!(img_turn.content(), "a photo of a clinic");
    }

    #[test]
    fn qa_gold_adversarial_and_labels() {
        let s = sample();
        assert_eq!(s.qa[0].gold_answer(), "A puppy named Biscuit");
        assert!(!s.qa[0].is_adversarial());
        assert_eq!(category_label(s.qa[0].category), "single-hop");

        assert_eq!(s.qa[1].gold_answer(), "1");
        assert_eq!(category_label(s.qa[1].category), "temporal");

        assert!(s.qa[2].is_adversarial());
        assert_eq!(s.qa[2].gold_answer(), "Not mentioned");
    }

    #[test]
    fn to_bench_sample_labels_turns_and_questions() {
        let bench = to_bench_sample(&sample());
        assert_eq!(bench.sample_id.as_deref(), Some("conv-1"));
        // Turns are speaker-labeled; the image turn used its caption.
        assert_eq!(
            bench.conversation.sessions[0].turns[0],
            "Caroline: I just adopted a puppy named Biscuit!"
        );
        assert_eq!(
            bench.conversation.sessions[1].turns[1],
            "Caroline: a photo of a clinic"
        );
        // Questions carry resolved labels + the abstain flag.
        assert_eq!(bench.questions[0].category, "single-hop");
        assert!(!bench.questions[0].adversarial);
        assert!(bench.questions[2].adversarial);
        assert_eq!(bench.questions[2].gold_answer, "Not mentioned");
    }
}
