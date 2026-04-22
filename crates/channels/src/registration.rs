//! Per-channel interactive registration flows.
//!
//! Each channel owns the prompts and validation used by `aura channel
//! add` to collect the credentials it needs. Flows stay
//! framework-agnostic by driving a caller-supplied [`Prompter`], so the
//! CLI can reuse its existing raw-terminal helpers without the channel
//! crate pulling in a TTY library.

use std::sync::Arc;

use aura_model::ChannelType;

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
}
