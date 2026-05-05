//! Interactive first-run / reconfigure wizard (`aura setup`). Bootstraps
//! the workspace, mints the master encryption key, and walks step
//! primitives that `aura llm add` / `aura channel add` also delegate to.
//! See `docs/modules/setup.md` for the design.

#![deny(unsafe_code)]

pub mod bootstrap;
pub mod error;
pub mod flow;
pub mod prompt;
pub mod runner;
mod tty;

pub use bootstrap::{SetupContext, bootstrap_workspace_if_needed};
pub use error::{Result, SetupError};
pub use prompt::Prompter;
pub use runner::{
    EndAction, SetupMode, SetupOutcome, launch_gateway_and_open_browser, pick_end_action,
    print_exit_hint, run, run_full, run_quick,
};
pub use tty::TtyPrompter;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
