//! Interactive single/multi-select prompts for CLI commands. Thin
//! delegations to `baybo_setup::TtyPrompter` — the one line-input prompt
//! implementation in the workspace — so the numbered-menu rendering lives
//! in exactly one place. `?` / `map_err(Into::into)` map `SetupError` →
//! `CliError` (see `crate::error`); `NotATerminal` / `Prompt` land on
//! `CliError::Config`, matching what callers already expect.

use baybo_setup::{Prompter, TtyPrompter};

use crate::error::Result;

/// Single-select numbered menu; returns the chosen index. Prints a
/// `1) … 2) …` list and reads the picked number, re-prompting on invalid
/// input. `Ctrl-C` (SIGINT) aborts the process; `Ctrl-D` (EOF) surfaces a
/// prompt error.
pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    let mut prompter = TtyPrompter::new()?;
    prompter.select(label, options).map_err(Into::into)
}

/// Multi-select numbered menu; returns the checked indices in ascending
/// order. `initial[i]` seeds row `i`'s checked state and is the set kept
/// when the operator submits an empty line; entering `0` (or `none`)
/// selects nothing.
pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    let mut prompter = TtyPrompter::new()?;
    prompter
        .multi_select(label, options, initial)
        .map_err(Into::into)
}
