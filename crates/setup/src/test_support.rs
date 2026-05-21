//! Test fixtures gated behind `cfg(any(test, feature = "test-support"))`
//! so they never ship in release builds.

use std::collections::VecDeque;

use crate::error::{Result, SetupError};
use crate::prompt::Prompter;

/// Scripted `Prompter` for unit tests. Each method dequeues from its
/// own scripted-answer queue; running out of answers panics so a
/// missing scripted line shows up as a loud test failure rather than a
/// silent hang.
#[derive(Debug, Default)]
pub struct MockPrompter {
    pub selects: VecDeque<usize>,
    pub multi_selects: VecDeque<Vec<usize>>,
    pub texts: VecDeque<String>,
    pub confirms: VecDeque<bool>,
    pub passwords: VecDeque<String>,
}

impl MockPrompter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_select(mut self, idx: usize) -> Self {
        self.selects.push_back(idx);
        self
    }

    pub fn push_multi_select(mut self, idxs: Vec<usize>) -> Self {
        self.multi_selects.push_back(idxs);
        self
    }

    pub fn push_text(mut self, s: impl Into<String>) -> Self {
        self.texts.push_back(s.into());
        self
    }

    pub fn push_confirm(mut self, ok: bool) -> Self {
        self.confirms.push_back(ok);
        self
    }

    pub fn push_password(mut self, s: impl Into<String>) -> Self {
        self.passwords.push_back(s.into());
        self
    }
}

impl Prompter for MockPrompter {
    fn select(&mut self, label: &str, options: &[&str]) -> Result<usize> {
        let idx = self.selects.pop_front().unwrap_or_else(|| {
            panic!("MockPrompter: select({label:?}) ran out of scripted answers")
        });
        if idx >= options.len() {
            return Err(SetupError::Prompt(format!(
                "MockPrompter scripted index {idx} out of range for {} options",
                options.len()
            )));
        }
        Ok(idx)
    }

    fn multi_select(
        &mut self,
        label: &str,
        options: &[&str],
        _initial: &[bool],
    ) -> Result<Vec<usize>> {
        let picks = self.multi_selects.pop_front().unwrap_or_else(|| {
            panic!("MockPrompter: multi_select({label:?}) ran out of scripted answers")
        });
        for &idx in &picks {
            if idx >= options.len() {
                return Err(SetupError::Prompt(format!(
                    "MockPrompter scripted index {idx} out of range for {} options",
                    options.len()
                )));
            }
        }
        Ok(picks)
    }

    fn text(&mut self, label: &str, _default: &str) -> Result<String> {
        Ok(self
            .texts
            .pop_front()
            .unwrap_or_else(|| panic!("MockPrompter: text({label:?}) ran out of scripted answers")))
    }

    fn confirm(&mut self, label: &str, _default: bool) -> Result<bool> {
        Ok(self.confirms.pop_front().unwrap_or_else(|| {
            panic!("MockPrompter: confirm({label:?}) ran out of scripted answers")
        }))
    }

    fn password(&mut self, label: &str) -> Result<String> {
        Ok(self.passwords.pop_front().unwrap_or_else(|| {
            panic!("MockPrompter: password({label:?}) ran out of scripted answers")
        }))
    }
}
