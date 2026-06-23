//! Per-step interactive primitives shared between the setup wizard
//! and `baybo llm add` / `baybo channel add`. Each primitive runs its
//! own side effects (vault writes, libsql rows, sidecar registrations)
//! and mutates `BayboConfig` in memory; `baybo.json` is committed once
//! at the end (β2).

mod browser;
mod channel;
mod external_agents;
mod llm;

pub use browser::{BrowserStepOutcome, configure_browser_step};
pub use channel::{
    ChannelStepOutcome, REGISTRATION_TIMEOUT, RegisteredBot, configure_channel_step,
    run_registration,
};
pub use external_agents::{ExternalAgentsStepOutcome, configure_external_agents_step};
pub use llm::{LlmStepOutcome, configure_llm_step};

use crate::error::Result;
use crate::prompt::Prompter;

/// Common Add / Skip picker used by the LLM and channel steps. With
/// `allow_skip = false` the picker is suppressed and the step runs
/// unconditionally — that's the contract `baybo llm add` /
/// `baybo channel add` rely on.
pub(crate) fn pick_add_or_skip<P: Prompter>(
    prompter: &mut P,
    label: &str,
    add_label: &str,
    skip_label: &str,
    allow_skip: bool,
) -> Result<bool> {
    if !allow_skip {
        return Ok(true);
    }
    let options: &[&str] = &[add_label, skip_label];
    Ok(prompter.select(label, options)? == 0)
}
