//! The dataset-agnostic seam between a benchmark's on-disk format and the
//! ingest/run harness.
//!
//! A [`TestSet`] parses its own released dataset into the normalized
//! [`BenchSample`] IR that the whole harness (ingest hooks, settle, QA, judge,
//! report) is written against. Adding a memory benchmark (LongMemEval,
//! MSC, …) is therefore a new `TestSet` impl in a sibling module plus one
//! [`TestSetKind`] variant — nothing downstream changes.

use std::path::Path;

use anyhow::Result;
use clap::ValueEnum;

pub mod locomo;

/// A benchmark test set: names itself and loads its dataset file into normalized
/// samples. The name is both the `--testset` value and the scope-key prefix (see
/// [`crate::scope_user_id`]), so two test sets can never collide in a shared
/// external backend.
pub trait TestSet: Send + Sync {
    fn name(&self) -> &'static str;
    fn load(&self, path: &Path) -> Result<Vec<BenchSample>>;
}

/// The registered test sets. Add a variant here and a match arm in
/// [`TestSetKind::test_set`] to register a new one; the bins surface it through
/// `--testset` automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TestSetKind {
    Locomo,
}

impl TestSetKind {
    pub fn test_set(self) -> &'static dyn TestSet {
        match self {
            TestSetKind::Locomo => &locomo::Locomo,
        }
    }

    pub fn name(self) -> &'static str {
        self.test_set().name()
    }
}

/// One normalized sample: a long, multi-session conversation plus its QA pairs.
#[derive(Debug, Clone)]
pub struct BenchSample {
    pub sample_id: Option<String>,
    pub conversation: BenchConversation,
    pub questions: Vec<BenchQuestion>,
}

/// The conversation as ordered, dated sessions of speaker-labeled turns.
#[derive(Debug, Clone)]
pub struct BenchConversation {
    pub sessions: Vec<BenchSession>,
}

impl BenchConversation {
    /// Render every session as dated, speaker-labeled text — the oracle arm
    /// folds this into the QA prompt.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for session in &self.sessions {
            match &session.date_time {
                Some(date) => out.push_str(&format!("\n# Session ({date})\n")),
                None => out.push_str(&format!("\n# Session {}\n", session.index)),
            }
            for turn in &session.turns {
                out.push_str(turn);
                out.push('\n');
            }
        }
        out
    }

    /// The first turn's text, used to derive a recall settle-probe.
    pub fn first_turn(&self) -> Option<&str> {
        self.sessions
            .first()
            .and_then(|s| s.turns.first())
            .map(String::as_str)
    }
}

/// One dated session of the dialogue. `turns` are already rendered as
/// `"{speaker}: {text}"`, in order.
#[derive(Debug, Clone)]
pub struct BenchSession {
    pub index: usize,
    pub date_time: Option<String>,
    pub turns: Vec<String>,
}

impl BenchSession {
    /// Split the session's turns into `(user_input, assistant)` pairs for
    /// `on_turn_complete`. The user side of each pair is prefixed with the session
    /// date so temporal questions stay answerable; a trailing odd turn pairs with
    /// an empty assistant string.
    pub fn turn_pairs(&self) -> Vec<(String, String)> {
        let date = self.date_time.as_deref().unwrap_or("");
        let mut pairs = Vec::with_capacity(self.turns.len().div_ceil(2));
        let mut i = 0;
        while i < self.turns.len() {
            let user = if date.is_empty() {
                self.turns[i].clone()
            } else {
                format!("[{date}] {}", self.turns[i])
            };
            let assistant = self.turns.get(i + 1).cloned().unwrap_or_default();
            pairs.push((user, assistant));
            i += 2;
        }
        pairs
    }
}

/// One QA pair, normalized: gold answer as a string, an `adversarial` flag (the
/// agent is expected to abstain), and a human category label for per-category
/// reporting. The label is the test set's own vocabulary — opaque to the harness.
#[derive(Debug, Clone)]
pub struct BenchQuestion {
    pub question: String,
    pub gold_answer: String,
    pub adversarial: bool,
    pub category: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> BenchSession {
        BenchSession {
            index: 1,
            date_time: Some("1:56 pm on 8 May, 2023".to_string()),
            turns: vec![
                "Caroline: I just adopted a puppy named Biscuit!".to_string(),
                "Melanie: Aww, I'm allergic to dogs though.".to_string(),
            ],
        }
    }

    #[test]
    fn turn_pairs_prefix_date_on_user_side() {
        let pairs = session().turn_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0].0,
            "[1:56 pm on 8 May, 2023] Caroline: I just adopted a puppy named Biscuit!"
        );
        assert_eq!(pairs[0].1, "Melanie: Aww, I'm allergic to dogs though.");
    }

    #[test]
    fn odd_turn_count_pairs_with_empty_assistant() {
        let mut s = session();
        s.turns.push("Caroline: see you!".to_string());
        let pairs = s.turn_pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1].1, "");
    }

    #[test]
    fn render_and_first_turn() {
        let conv = BenchConversation {
            sessions: vec![session()],
        };
        assert_eq!(
            conv.first_turn(),
            Some("Caroline: I just adopted a puppy named Biscuit!")
        );
        let rendered = conv.render();
        assert!(rendered.contains("# Session (1:56 pm on 8 May, 2023)"));
        assert!(rendered.contains("Caroline: I just adopted a puppy named Biscuit!"));
    }
}
