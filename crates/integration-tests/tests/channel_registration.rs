//! End-to-end registration test that drives the actual Telegram
//! sidecar bundle (built and embedded by `crates/gateway/build.rs`)
//! through the production registration driver. Channel sidecars run
//! on the host's `bun` binary at runtime.
//!
//! Skipped when this build doesn't ship an embedded sidecar bundle —
//! that path is the only safe escape hatch in degraded CI environments
//! where the `bun build` step couldn't run or `pnpm install` wasn't
//! done.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Duration;

use aura_channels::Prompter;
use aura_gateway::{SidecarError, SidecarRuntime};
use aura_model::ChannelType;
use aura_setup::flow::run_registration;

const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// Materialize the telegram bundle exactly once per `cargo test`
/// run. The three tests below race on `SidecarRuntime::install()`
/// otherwise, decompressing into the same cache dir concurrently.
static SHARED_RUNTIME: OnceLock<Option<SidecarRuntime>> = OnceLock::new();

struct ScriptedPrompter {
    answers: VecDeque<String>,
}

impl ScriptedPrompter {
    fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
        }
    }
}

impl Prompter for ScriptedPrompter {
    fn input(&mut self, label: &str, _required: bool) -> anyhow::Result<String> {
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("ScriptedPrompter ran out of answers for '{label}'"))
    }
    fn password(&mut self, label: &str, _required: bool) -> anyhow::Result<String> {
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("ScriptedPrompter ran out of answers for '{label}'"))
    }
}

fn telegram_runtime() -> Option<&'static SidecarRuntime> {
    SHARED_RUNTIME
        .get_or_init(|| match SidecarRuntime::install() {
            Ok(runtime) => {
                if runtime.bundle_for("telegram").is_none() {
                    eprintln!("skipping: this build does not embed the telegram sidecar bundle");
                    return None;
                }
                Some(runtime)
            }
            Err(SidecarError::RuntimeMissing) => {
                eprintln!("skipping: degraded build with no embedded sidecars");
                None
            }
            Err(e) => panic!("install sidecar runtime: {e}"),
        })
        .as_ref()
}

#[tokio::test]
async fn telegram_bundle_completes_registration_with_dummy_token() {
    let Some(runtime) = telegram_runtime() else {
        return;
    };
    let ct = ChannelType::telegram();
    let mut prompter = ScriptedPrompter::new(["123456789:test-secret-aA0_-"]);

    let result = run_registration(runtime, &ct, &mut prompter, REGISTER_TIMEOUT)
        .await
        .expect("telegram registration should succeed");

    assert_eq!(result.bot_id, "123456789");
    assert_eq!(result.token, "123456789:test-secret-aA0_-");
}

#[tokio::test]
async fn telegram_bundle_rejects_token_without_colon() {
    let Some(runtime) = telegram_runtime() else {
        return;
    };
    let ct = ChannelType::telegram();
    let mut prompter = ScriptedPrompter::new(["not-a-real-token"]);

    let err = run_registration(runtime, &ct, &mut prompter, REGISTER_TIMEOUT)
        .await
        .expect_err("registration must fail for malformed token");
    let msg = err.to_string();
    assert!(
        msg.contains("telegram") || msg.contains("numeric"),
        "expected validation message in: {msg}"
    );
}

#[tokio::test]
async fn telegram_bundle_rejects_non_numeric_prefix() {
    let Some(runtime) = telegram_runtime() else {
        return;
    };
    let ct = ChannelType::telegram();
    let mut prompter = ScriptedPrompter::new(["abc:defghij"]);

    let err = run_registration(runtime, &ct, &mut prompter, REGISTER_TIMEOUT)
        .await
        .expect_err("registration must fail for non-numeric prefix");
    let msg = err.to_string();
    assert!(
        msg.contains("numeric") || msg.contains("invalid"),
        "expected numeric-prefix validation message in: {msg}"
    );
}
