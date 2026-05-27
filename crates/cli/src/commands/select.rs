//! Interactive single/multi-select pickers for CLI commands. Thin
//! delegations to `aura_setup::TtyPrompter` — the one raw-mode picker
//! implementation in the workspace — so the crossterm rendering lives in
//! exactly one place. `?` / `map_err(Into::into)` map `SetupError` →
//! `CliError` (see `crate::error`); `Cancelled` / `NotATerminal` land on
//! `CliError::Config`, matching what callers already expect.

use aura_setup::{Prompter, TtyPrompter};

use crate::error::Result;

/// Single-select radio list; returns the chosen index. `Esc` / `Ctrl-C`
/// raise a cancellation error rather than returning an index.
pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    let mut prompter = TtyPrompter::new()?;
    prompter.select(label, options).map_err(Into::into)
}

/// Multi-select checkbox list; returns the checked indices in ascending
/// order. `initial[i]` seeds row `i`'s checked state; an empty confirm
/// returns an empty vec (distinct from `Esc` / `Ctrl-C` cancellation).
pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    let mut prompter = TtyPrompter::new()?;
    prompter
        .multi_select(label, options, initial)
        .map_err(Into::into)
}
