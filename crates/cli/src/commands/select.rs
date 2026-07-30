//! Interactive single/multi-select prompts for CLI commands. Thin
//! delegations to `baybo_setup::TtyPrompter` so the arrow-key menu rendering
//! lives in exactly one place. `?` / `map_err(Into::into)` map `SetupError` →
//! `CliError` (see `crate::error`); `NotATerminal` / `Prompt` land on
//! `CliError::Config`, matching what callers already expect.

use baybo_setup::{Prompter, TtyPrompter};

use crate::error::Result;

/// Single-select arrow-key menu; returns the chosen index. Up/down moves
/// the cursor and Enter confirms. Ctrl-C / Ctrl-D abort the prompt.
pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    let mut prompter = TtyPrompter::new()?;
    prompter.select(label, options).map_err(Into::into)
}

/// Multi-select arrow-key menu; returns the checked indices in ascending
/// order. `initial[i]` seeds row `i`'s checked state; up/down moves,
/// Space toggles, and Enter confirms.
pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    let mut prompter = TtyPrompter::new()?;
    prompter
        .multi_select(label, options, initial)
        .map_err(Into::into)
}
