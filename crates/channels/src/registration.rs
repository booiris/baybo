//! Per-channel interactive registration flows.
//!
//! Each channel owns the prompts and validation used by `aura channel
//! add` to collect the credentials it needs. Flows stay
//! framework-agnostic by driving a caller-supplied [`Prompter`], so the
//! CLI can reuse its existing raw-terminal helpers without the channel
//! crate pulling in a TTY library.

use std::sync::Arc;

use aura_model::ChannelType;
use serde::{Deserialize, Serialize};

pub trait Prompter {
    /// Prompt for a single line of text input. When `required` is
    /// true, empty responses re-prompt instead of returning `""`.
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String>;

    /// Prompt for a masked secret. `required` semantics match [`input`].
    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String>;
}

/// Credentials produced by a channel-specific registration flow.
#[derive(Debug, Clone)]
pub struct RegistrationResult {
    /// Stable identifier used by the channel to address the bot.
    /// Written to libsql and surfaced by `aura channel bots`.
    pub bot_id: String,
    /// Secrets to persist to the vault as `(secret_name, value)` pairs.
    /// Each channel owns its own naming convention.
    pub secrets: Vec<(String, String)>,
}

pub trait RegistrationFlow: Send + Sync {
    fn channel_type(&self) -> ChannelType;
    fn display_name(&self) -> &'static str;
    fn prompt(&self, prompter: &mut dyn Prompter) -> anyhow::Result<RegistrationResult>;
}

/// Static catalog of channel registration flows the CLI can offer.
///
/// Weixin is intentionally absent: its registration flow requires
/// spawning the bundled sidecar in QR-login mode, which means the
/// concrete [`WeixinLoginRunner`] is wired up in the CLI crate (where
/// the gateway's [`SidecarRuntime`] is available). The CLI composes
/// this catalog with a [`WeixinRegistration`] when it has everything
/// needed to run login.
pub fn builtin_registration_flows() -> Vec<Arc<dyn RegistrationFlow>> {
    vec![Arc::new(TelegramRegistration)]
}

pub struct TelegramRegistration;

impl RegistrationFlow for TelegramRegistration {
    fn channel_type(&self) -> ChannelType {
        ChannelType::telegram()
    }

    fn display_name(&self) -> &'static str {
        "telegram"
    }

    fn prompt(&self, prompter: &mut dyn Prompter) -> anyhow::Result<RegistrationResult> {
        let token = prompter.password("bot token: ", true)?;
        let bot_id = parse_telegram_bot_id(&token)?.to_string();
        let secret_key = format!("channel.telegram.bot.{bot_id}.token");
        Ok(RegistrationResult {
            bot_id,
            secrets: vec![(secret_key, token)],
        })
    }
}

fn parse_telegram_bot_id(token: &str) -> anyhow::Result<&str> {
    let (prefix, suffix) = token.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("telegram bot tokens must look like `<numeric_id>:<secret>`")
    })?;
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("telegram bot id (the part before `:`) must be a non-empty numeric string");
    }
    if suffix.is_empty() {
        anyhow::bail!("telegram bot token (the part after `:`) must not be empty");
    }
    Ok(prefix)
}

/// Authentication blob persisted into the vault as
/// `channel.weixin.bot.<bot_id>.token`. The weixin sidecar reads this
/// string verbatim from `StartBotCommand.token` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeixinAuthBlob {
    pub version: u32,
    #[serde(rename = "botToken")]
    pub bot_token: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Runs the weixin interactive QR login, blocking until it completes,
/// and returns the resulting auth blob. Implemented by the CLI crate
/// so the `aura-channels` crate stays framework-agnostic (doesn't need
/// to depend on `aura-gateway` for [`SidecarRuntime`]).
pub trait WeixinLoginRunner: Send + Sync {
    fn run_login(&self) -> anyhow::Result<WeixinAuthBlob>;
}

pub struct WeixinRegistration {
    runner: Arc<dyn WeixinLoginRunner>,
}

impl WeixinRegistration {
    pub fn new(runner: Arc<dyn WeixinLoginRunner>) -> Self {
        Self { runner }
    }
}

impl RegistrationFlow for WeixinRegistration {
    fn channel_type(&self) -> ChannelType {
        ChannelType::weixin()
    }

    fn display_name(&self) -> &'static str {
        "weixin"
    }

    fn prompt(&self, _prompter: &mut dyn Prompter) -> anyhow::Result<RegistrationResult> {
        // The runner owns its own interactive UI (QR rendering in the
        // terminal); the `Prompter` abstraction is unused here.
        let blob = self.runner.run_login()?;
        let bot_id = blob.account_id.clone();
        let secret_key = format!("channel.weixin.bot.{bot_id}.token");
        let value = serde_json::to_string(&blob)
            .map_err(|e| anyhow::anyhow!("serialize weixin auth blob: {e}"))?;
        Ok(RegistrationResult {
            bot_id,
            secrets: vec![(secret_key, value)],
        })
    }
}

pub const WEIXIN_LOGIN_RESULT_MARKER: &str = "AURA_WEIXIN_LOGIN_RESULT:";

/// Parse one line of sidecar stdout. Returns `Some(blob)` when the
/// line carries the result marker and the JSON payload is a valid
/// `WeixinAuthBlob`. The CLI's subprocess driver calls this on every
/// stdout line until it matches; any line that doesn't match is
/// forwarded to the operator's terminal (QR output).
pub fn parse_login_result_line(line: &str) -> anyhow::Result<Option<WeixinAuthBlob>> {
    let Some(rest) = line.strip_prefix(WEIXIN_LOGIN_RESULT_MARKER) else {
        return Ok(None);
    };
    let blob: WeixinAuthBlob = serde_json::from_str(rest.trim())
        .map_err(|e| anyhow::anyhow!("parse weixin login result JSON: {e}"))?;
    if blob.version != 1 {
        anyhow::bail!("unsupported weixin auth blob version: {}", blob.version);
    }
    if blob.bot_token.is_empty() || blob.base_url.is_empty() || blob.account_id.is_empty() {
        anyhow::bail!("weixin auth blob missing required fields");
    }
    Ok(Some(blob))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakePrompter {
        answers: VecDeque<String>,
    }

    impl FakePrompter {
        fn new(answers: impl IntoIterator<Item = impl Into<String>>) -> Self {
            Self {
                answers: answers.into_iter().map(Into::into).collect(),
            }
        }
    }

    impl Prompter for FakePrompter {
        fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
            let value = self
                .answers
                .pop_front()
                .unwrap_or_else(|| panic!("FakePrompter ran out of answers for input({label:?})"));
            if required && value.is_empty() {
                anyhow::bail!("`{}` is required", label.trim_end_matches(": "));
            }
            Ok(value)
        }

        fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
            let value = self.answers.pop_front().unwrap_or_else(|| {
                panic!("FakePrompter ran out of answers for password({label:?})")
            });
            if required && value.is_empty() {
                anyhow::bail!("`{}` is required", label.trim_end_matches(": "));
            }
            Ok(value)
        }
    }

    #[test]
    fn valid_token_yields_bot_id_and_secret() {
        let mut prompter = FakePrompter::new(["123456789:AAE-abcDEF_xyz"]);
        let out = TelegramRegistration.prompt(&mut prompter).unwrap();
        assert_eq!(out.bot_id, "123456789");
        assert_eq!(out.secrets.len(), 1);
        assert_eq!(out.secrets[0].0, "channel.telegram.bot.123456789.token");
        assert_eq!(out.secrets[0].1, "123456789:AAE-abcDEF_xyz");
    }

    #[test]
    fn empty_token_is_rejected() {
        let mut prompter = FakePrompter::new([""]);
        let err = TelegramRegistration.prompt(&mut prompter).unwrap_err();
        assert!(
            err.to_string().contains("required"),
            "expected required error, got: {err}"
        );
    }

    #[test]
    fn token_without_colon_is_rejected() {
        let mut prompter = FakePrompter::new(["not-a-real-token"]);
        let err = TelegramRegistration.prompt(&mut prompter).unwrap_err();
        assert!(err.to_string().contains("numeric_id"), "got: {err}");
    }

    #[test]
    fn token_with_non_numeric_prefix_is_rejected() {
        let mut prompter = FakePrompter::new(["abc:defghij"]);
        let err = TelegramRegistration.prompt(&mut prompter).unwrap_err();
        assert!(err.to_string().contains("numeric"), "got: {err}");
    }

    #[test]
    fn token_with_empty_suffix_is_rejected() {
        let mut prompter = FakePrompter::new(["123:"]);
        let err = TelegramRegistration.prompt(&mut prompter).unwrap_err();
        assert!(err.to_string().contains("after `:`"), "got: {err}");
    }

    #[test]
    fn token_with_empty_prefix_is_rejected() {
        let mut prompter = FakePrompter::new([":secret"]);
        let err = TelegramRegistration.prompt(&mut prompter).unwrap_err();
        assert!(err.to_string().contains("numeric"), "got: {err}");
    }

    fn sample_blob() -> WeixinAuthBlob {
        WeixinAuthBlob {
            version: 1,
            bot_token: "bot-token-xyz".into(),
            base_url: "https://idc1.ilinkai.weixin.qq.com".into(),
            user_id: "u123@im.wechat".into(),
            account_id: "b0f5860fdecb-im-bot".into(),
            created_at: "2026-04-24T00:00:00Z".into(),
        }
    }

    #[test]
    fn parse_login_result_line_accepts_valid_marker() {
        let blob = sample_blob();
        let line = format!(
            "{WEIXIN_LOGIN_RESULT_MARKER}{}\n",
            serde_json::to_string(&blob).unwrap()
        );
        let out = parse_login_result_line(&line).unwrap().unwrap();
        assert_eq!(out, blob);
    }

    #[test]
    fn parse_login_result_line_ignores_unrelated_lines() {
        assert!(parse_login_result_line("").unwrap().is_none());
        assert!(parse_login_result_line("hello world\n").unwrap().is_none());
        assert!(
            parse_login_result_line("AURA_WEIXIN_LOGIN_WARN: hmm\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parse_login_result_line_rejects_wrong_version() {
        let mut v = serde_json::to_value(sample_blob()).unwrap();
        v["version"] = serde_json::json!(2);
        let line = format!("{WEIXIN_LOGIN_RESULT_MARKER}{v}\n");
        let err = parse_login_result_line(&line).unwrap_err();
        assert!(err.to_string().contains("version"), "got: {err}");
    }

    #[test]
    fn parse_login_result_line_rejects_missing_fields() {
        let mut v = serde_json::to_value(sample_blob()).unwrap();
        v["botToken"] = serde_json::json!("");
        let line = format!("{WEIXIN_LOGIN_RESULT_MARKER}{v}\n");
        let err = parse_login_result_line(&line).unwrap_err();
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn parse_login_result_line_rejects_bad_json() {
        let line = format!("{WEIXIN_LOGIN_RESULT_MARKER}{{not json\n");
        let err = parse_login_result_line(&line).unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    struct StubRunner(WeixinAuthBlob);
    impl WeixinLoginRunner for StubRunner {
        fn run_login(&self) -> anyhow::Result<WeixinAuthBlob> {
            Ok(self.0.clone())
        }
    }

    struct FailingRunner;
    impl WeixinLoginRunner for FailingRunner {
        fn run_login(&self) -> anyhow::Result<WeixinAuthBlob> {
            anyhow::bail!("login cancelled")
        }
    }

    #[test]
    fn weixin_registration_builds_secret_and_bot_id() {
        let blob = sample_blob();
        let reg = WeixinRegistration::new(Arc::new(StubRunner(blob.clone())));
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let out = reg.prompt(&mut prompter).unwrap();
        assert_eq!(out.bot_id, blob.account_id);
        assert_eq!(out.secrets.len(), 1);
        assert_eq!(
            out.secrets[0].0,
            format!("channel.weixin.bot.{}.token", blob.account_id),
        );
        let round: WeixinAuthBlob = serde_json::from_str(&out.secrets[0].1).unwrap();
        assert_eq!(round, blob);
    }

    #[test]
    fn weixin_registration_surfaces_runner_error() {
        let reg = WeixinRegistration::new(Arc::new(FailingRunner));
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let err = reg.prompt(&mut prompter).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "got: {err}");
    }
}
