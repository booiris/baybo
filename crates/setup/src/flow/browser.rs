//! Browser tool tiered prompt — full setup only.

use std::time::Duration;

use aura_config::AuraConfig;

use crate::error::Result;
use crate::prompt::Prompter;

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserStepOutcome {
    Enabled { docker: bool, sandbox: bool },
    Disabled,
}

pub async fn configure_browser_step<P: Prompter>(
    prompter: &mut P,
    config: &mut AuraConfig,
) -> Result<BrowserStepOutcome> {
    if !prompter.confirm(
        "Enable agent web browsing? Adds the browser/* MCP tools and lazily \
         downloads Chrome on first use.",
        false,
    )? {
        config.browser.enable = false;
        return Ok(BrowserStepOutcome::Disabled);
    }

    let docker = match probe_docker_for_prompt().await {
        Some(true) => prompter.confirm(
            "Run Chrome inside Docker for better real-user simulation? \
             (skip if unsure — the host-headless path works for most agents)",
            false,
        )?,
        Some(false) | None => false,
    };

    // Docker mode upstream ignores `sandbox`; only ask when host-headless.
    let sandbox = if docker {
        false
    } else {
        prompter.confirm(
            "Enable Chrome renderer sandbox? Requires user-namespace support; \
             leave off if you're on a hardened/CI host or unsure.",
            false,
        )?
    };

    config.browser.enable = true;
    config.browser.docker.enable = docker;
    config.browser.sandbox = sandbox;

    Ok(BrowserStepOutcome::Enabled { docker, sandbox })
}

/// Returns `Some(true)` if Docker is reachable, `Some(false)` if it's
/// installed but the daemon is unhealthy, and `None` to skip the prompt
/// entirely (binary missing, macOS, or probe timeout).
async fn probe_docker_for_prompt() -> Option<bool> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    use std::process::Stdio;
    use tokio::process::Command;
    let probe = Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(status)) if status.success() => Some(true),
        Ok(Ok(_)) => Some(false),
        Ok(Err(_)) => None,
        Err(_) => {
            tracing::info!("browser step: docker probe timed out; skipping prompt");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockPrompter;
    use aura_config::AuraConfig;

    #[tokio::test]
    async fn declining_master_keeps_defaults() {
        let mut prompter = MockPrompter::new().push_confirm(false);
        let mut cfg = AuraConfig::default();
        let outcome = configure_browser_step(&mut prompter, &mut cfg)
            .await
            .unwrap();
        assert_eq!(outcome, BrowserStepOutcome::Disabled);
        assert!(!cfg.browser.enable);
    }

    #[tokio::test]
    async fn enabling_without_docker_asks_about_sandbox() {
        // Skip when a real Docker daemon would consume an extra
        // confirm answer — the fixture only scripts two.
        if cfg!(target_os = "linux") && probe_docker_for_prompt().await == Some(true) {
            return;
        }
        let mut prompter = MockPrompter::new().push_confirm(true).push_confirm(false);
        let mut cfg = AuraConfig::default();
        let outcome = configure_browser_step(&mut prompter, &mut cfg)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            BrowserStepOutcome::Enabled {
                docker: false,
                sandbox: false
            }
        );
        assert!(cfg.browser.enable);
        assert!(!cfg.browser.docker.enable);
        assert!(!cfg.browser.sandbox);
    }
}
