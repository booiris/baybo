use crate::error::Result;

/// Driver for one interactive turn. Real CLI uses
/// [`crate::TtyPrompter`]; tests use
/// [`crate::test_support::MockPrompter`].
///
/// `Send` is required so the channel-step adapter can satisfy
/// `aura_channels::registration::Prompter`'s own `Send` bound.
pub trait Prompter: Send {
    fn select(&mut self, label: &str, options: &[&str]) -> Result<usize>;
    /// Empty submission returns the default verbatim.
    fn text(&mut self, label: &str, default: &str) -> Result<String>;
    /// `[Y/n]` when `default = true`, `[y/N]` otherwise.
    fn confirm(&mut self, label: &str, default: bool) -> Result<bool>;
    fn password(&mut self, label: &str) -> Result<String>;
}
