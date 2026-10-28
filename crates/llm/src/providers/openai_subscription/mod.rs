//! Sign in with ChatGPT subscription provider. See
//! `docs/modules/llm-openai-subscription.md` for design rationale.

mod completion_model;
mod factory;
mod oauth;
mod reasoning;
mod refresh_coordinator;
mod token_bundle;
mod token_store;

/// Provider id shared across CLI gating, factory registration, and the
/// `LlmConfig.provider` field. Single source of truth so the literal
/// "openai-subscription" doesn't drift across files.
pub const PROVIDER_NAME: &str = "openai-subscription";
pub const MANAGE_USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// Vault key the OAuth bundle is persisted under (single profile).
pub const VAULT_KEY_TOKENS: &str = "llm.openai-subscription.tokens";
/// Model the setup wizard seeds as a subscription entry's `lite_model`, when
/// the account's catalog lists it: the middle tier keeps auxiliary calls off
/// the flagship's quota.
pub const LITE_MODEL: &str = "gpt-5.6-terra";

pub use completion_model::{DEFAULT_BASE_URL, OpenAiSubscriptionCompletionModel};
pub use factory::{OpenAiSubscriptionProviderFactory, UNSAFE_BASE_URL_ENV_VAR};
pub use oauth::{
    CALLBACK_PORT, ISSUER, LoginCallback, RefreshError, pkce_login, pkce_login_with_callback,
    refresh, revoke,
};
pub use reasoning::allowed_efforts_for;
pub use token_bundle::OAuthTokenBundle;
pub use token_store::VaultTokenStore;
