use baybo_config::{
    BayboConfig, ClaudeConfig, CodexConfig, ConfigError, DiscordChannelConfig,
    ExternalAgentsConfig, GeminiConfig, LlmEntry, LlmEntryName, LlmModelSpec, PermissionPolicy,
    ProxyConfig, TelegramChannelConfig,
};
use baybo_model::{ExternalAgentKind, ModelTier};

fn has_field(errors: &[baybo_config::ValidationError], field: &str) -> bool {
    errors.iter().any(|e| e.field == field)
}

fn unwrap_validation(err: ConfigError) -> Vec<baybo_config::ValidationError> {
    match err {
        ConfigError::Validation(v) => v,
        other => panic!("expected Validation error, got {other:?}"),
    }
}

#[test]
fn default_config_is_valid() {
    let config = BayboConfig::default();
    config.validate().expect("default config should validate");
}

#[test]
fn empty_json_uses_defaults() {
    let config = BayboConfig::load_from_str("{}").expect("empty object should parse");
    assert_eq!(config, BayboConfig::default());
}

#[test]
fn permission_defaults_to_auto() {
    let config = BayboConfig::default();
    assert_eq!(config.permission, PermissionPolicy::Auto);
}

#[test]
fn parses_permission_policy() {
    let config = BayboConfig::load_from_str(
        r#"{
            "permission": "Manual"
        }"#,
    )
    .expect("permission config should parse");

    assert_eq!(config.permission, PermissionPolicy::Manual);
}

#[test]
fn parses_free_permission_and_legacy_aliases() {
    let free = BayboConfig::load_from_str(r#"{ "permission": "Free" }"#)
        .expect("free permission should parse");
    assert_eq!(free.permission, PermissionPolicy::Free);

    let open = BayboConfig::load_from_str(r#"{ "permission": "open" }"#)
        .expect("legacy open permission alias should parse");
    assert_eq!(open.permission, PermissionPolicy::Free);

    let legacy = BayboConfig::load_from_str(r#"{ "permission": "none" }"#)
        .expect("legacy none permission alias should parse");
    assert_eq!(legacy.permission, PermissionPolicy::Free);
}

#[test]
fn example_config_uses_top_level_permission() {
    let example = include_str!("../../../baybo.example.json");
    let config = BayboConfig::load_from_str(example).expect("example config should parse");
    assert_eq!(config.permission, PermissionPolicy::Auto);

    let raw: serde_json::Value = serde_json::from_str(example).expect("example config is json");
    assert_eq!(raw.get("permission").and_then(|v| v.as_str()), Some("auto"));
    assert!(
        raw.get("safety").is_none(),
        "example config must not use the legacy safety wrapper"
    );
    assert!(
        raw.get("sandbox").is_none(),
        "example config must not use the legacy sandbox wrapper"
    );
}

#[test]
fn invalid_json_returns_parse_error() {
    let err = BayboConfig::load_from_str("not json").unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

fn entry(name: &str) -> LlmEntry {
    LlmEntry {
        name: name.into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        model_list: Vec::new(),
        lite_model: None,
        api_key_env: None,
        base_url: None,
        reasoning_effort: None,
    }
}

fn config_with_default_entry() -> BayboConfig {
    BayboConfig {
        llm: vec![entry("openai")],
        default_llm: "openai".into(),
        ..BayboConfig::default()
    }
}

#[test]
fn empty_provider_fails_validation() {
    let mut c = config_with_default_entry();
    c.llm[0].provider = String::new();
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].provider"));
}

#[test]
fn empty_model_fails_validation() {
    let mut c = config_with_default_entry();
    c.llm[0].model = String::new();
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].model"));
}

/// Caught in `validate()` rather than at client-build time, so a reload
/// dry-run rejects the typo without constructing any client.
#[test]
fn lite_model_outside_model_list_fails_validation() {
    let mut c = config_with_default_entry();
    c.llm[0].lite_model = Some("gpt-4o-nano".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].lite_model"));
}

#[test]
fn lite_model_naming_a_listed_model_validates() {
    let mut c = config_with_default_entry();
    c.llm[0].model_list = vec![LlmModelSpec::bare("gpt-4o-nano")];
    c.llm[0].lite_model = Some("gpt-4o-nano".into());
    assert!(c.validate().is_ok());
}

/// The entry's own default model is always serveable, so naming it as
/// the lite model is legal even with an empty `model_list`.
///
/// Load-bearing for first-run setup: `configure_llm_step` seeds
/// `lite_model` to the entry's own model precisely so the knob is visible
/// in the generated `baybo.json`, and it writes no `model_list` at all. If
/// `models()` ever stopped prepending the default, every wizard-created
/// config would fail to load.
#[test]
fn lite_model_may_name_the_entry_default() {
    let mut c = config_with_default_entry();
    c.llm[0].lite_model = Some(c.llm[0].model.clone());
    assert!(c.validate().is_ok());
}

/// The admin `PUT` already rejects a zero window; the file path must
/// too, or it accepts what the API refuses. A zero window is not inert —
/// it collapses `compression_threshold * context_window` and zeroes
/// WebFetch's summariser budget.
#[test]
fn zero_context_window_fails_validation() {
    let mut c = config_with_default_entry();
    let mut spec = LlmModelSpec::bare("gpt-4o-mini");
    spec.context_window = Some(0);
    c.llm[0].model_list = vec![spec];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].model_list[0].context_window"));
}

#[test]
fn bad_base_url_fails_validation() {
    let mut c = config_with_default_entry();
    c.llm[0].base_url = Some("ftp://x".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].base_url"));
}

#[test]
fn valid_proxy_passes_validation() {
    let mut c = config_with_default_entry();
    c.proxy = Some(ProxyConfig {
        url: "socks5://127.0.0.1:1080".into(),
        no_proxy: Some(vec![".internal".into()]),
    });
    assert!(c.validate().is_ok());
}

#[test]
fn proxy_unsupported_scheme_fails_validation() {
    let mut c = config_with_default_entry();
    c.proxy = Some(ProxyConfig {
        url: "ftp://proxy:21".into(),
        no_proxy: None,
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "proxy.url"));
}

#[test]
fn proxy_empty_url_fails_validation() {
    let mut c = config_with_default_entry();
    c.proxy = Some(ProxyConfig {
        url: "  ".into(),
        no_proxy: None,
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "proxy.url"));
}

#[test]
fn duplicate_entry_names_fail_validation() {
    let mut c = config_with_default_entry();
    c.llm.push(entry("openai"));
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[1].name"));
}

#[test]
fn empty_llm_list_is_valid() {
    // Fresh-install state: no entries, default-llm empty, runtime
    // surfaces a useful error only when something actually needs the
    // LLM.
    let c = BayboConfig::default();
    assert!(c.llm.is_empty());
    assert!(c.default_llm.as_str().is_empty());
    assert!(c.validate().is_ok());
}

#[test]
fn default_llm_required_when_entries_exist() {
    let c = BayboConfig {
        llm: vec![entry("openai")],
        default_llm: LlmEntryName::default(),
        ..BayboConfig::default()
    };
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "default-llm"));
}

#[test]
fn default_llm_must_reference_existing_entry() {
    let c = BayboConfig {
        llm: vec![entry("openai")],
        default_llm: "missing".into(),
        ..BayboConfig::default()
    };
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "default-llm"));
}

#[test]
fn model_tier_mapping_to_unknown_entry_fails_validation() {
    let mut c = config_with_default_entry();
    c.agent
        .model_tiers
        .insert(ModelTier::Lite, "missing".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.model_tiers"));
}

#[test]
fn model_tier_mapping_to_existing_entry_is_valid() {
    let mut c = config_with_default_entry();
    c.agent.model_tiers.insert(ModelTier::Lite, "openai".into());
    c.validate()
        .expect("model_tier pointing at a real llm entry is valid");
}

#[test]
fn compression_threshold_bounds() {
    // zero is invalid
    let mut c = BayboConfig::default();
    c.agent.context.compression_threshold = 0.0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.context.compression_threshold"));

    // above 1 is invalid
    let mut c = BayboConfig::default();
    c.agent.context.compression_threshold = 1.5;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.context.compression_threshold"));

    // exactly 1 is valid
    let mut c = BayboConfig::default();
    c.agent.context.compression_threshold = 1.0;
    assert!(c.validate().is_ok());
}

#[test]
fn max_iterations_bounds() {
    let mut c = BayboConfig::default();
    c.agent.max_iterations = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.max_iterations"));

    let mut c = BayboConfig::default();
    c.agent.max_iterations = 1001;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.max_iterations"));
}

#[test]
fn channel_buffer_bounds() {
    let mut c = BayboConfig::default();
    c.channels.message_buffer_size = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.message_buffer_size"));

    let mut c = BayboConfig::default();
    c.channels.message_buffer_size = 100_000;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.message_buffer_size"));
}

#[test]
fn spending_limits_must_be_positive() {
    let mut c = BayboConfig::default();
    c.cost.spending_limits.daily_usd = Some(baybo_model::MicroUsd::from_usd_decimal(-1.0));
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.spending_limits.daily_usd"));
}

#[test]
fn daily_cannot_exceed_monthly_spend() {
    let mut c = BayboConfig::default();
    c.cost.spending_limits.daily_usd = Some(baybo_model::MicroUsd::from_usd_decimal(100.0));
    c.cost.spending_limits.monthly_usd = Some(baybo_model::MicroUsd::from_usd_decimal(50.0));
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.spending_limits.daily_usd"));
}

#[test]
fn rate_limit_fields_must_be_positive() {
    let mut c = BayboConfig::default();
    c.cost.rate_limit.max_requests = 0;
    c.cost.rate_limit.window_secs = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.rate_limit.max_requests"));
    assert!(has_field(&errors, "cost.rate_limit.window_secs"));
}

#[test]
fn full_roundtrip_via_json() {
    let config = BayboConfig::default();
    let json = serde_json::to_string(&config).expect("serialize");
    let parsed = BayboConfig::load_from_str(&json).expect("reparse");
    assert_eq!(parsed, config);
}

#[test]
fn load_from_file_reads_and_parses() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join("baybo-config-test.json");
    std::fs::write(
        &tmp,
        r#"{
            "llm": [
                {"name":"anthropic","provider":"anthropic","model":"claude-sonnet-4-6"}
            ],
            "default-llm": "anthropic"
        }"#,
    )?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let config = rt.block_on(BayboConfig::load_from_file(&tmp))?;
    assert_eq!(config.llm.len(), 1);
    assert_eq!(config.llm[0].provider, "anthropic");
    assert_eq!(config.llm[0].model, "claude-sonnet-4-6");
    assert_eq!(config.default_llm, "anthropic");
    std::fs::remove_file(&tmp).ok();
    Ok(())
}

#[test]
fn missing_file_returns_file_read_error() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = rt
        .block_on(BayboConfig::load_from_file(std::path::Path::new(
            "/nonexistent/path/baybo-config-missing.json",
        )))
        .unwrap_err();
    assert!(matches!(err, ConfigError::FileRead { .. }));
}

#[test]
fn telegram_channel_disabled_flag_is_rejected() {
    let mut c = BayboConfig::default();
    c.channels.telegram = Some(TelegramChannelConfig {
        enabled: false,
        bot_token_env: "TG_TOKEN".into(),
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.telegram"));
}

#[test]
fn discord_channel_disabled_flag_is_rejected() {
    let mut c = BayboConfig::default();
    c.channels.discord = Some(DiscordChannelConfig {
        enabled: false,
        bot_token_env: "DC_TOKEN".into(),
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.discord"));
}

#[test]
fn encryption_key_file_is_required() {
    let mut c = BayboConfig::default();
    c.security.encryption_key_file = None;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "security.encryption_key_file"));

    c.security.encryption_key_file = Some("/tmp/key".into());
    assert!(c.validate().is_ok());
}

#[test]
fn workspace_path_rejects_relative_value() {
    let mut c = BayboConfig::default();
    c.workspace.path = "./.baybo".into();
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "workspace.path"));
}

#[test]
fn encryption_key_file_rejects_relative_value() {
    let mut c = BayboConfig::default();
    c.security.encryption_key_file = Some("relative/key".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "security.encryption_key_file"));
}

#[test]
fn browser_paths_reject_relative_values() {
    let mut c = BayboConfig::default();
    c.browser.chrome_path = Some("relative/chrome".into());
    c.browser.profile_dir = Some("relative/profile".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "browser.chrome_path"));
    assert!(has_field(&errors, "browser.profile_dir"));
}

#[test]
fn llm_api_key_env_rejects_empty_string() {
    let mut c = config_with_default_entry();
    c.llm[0].api_key_env = Some(String::new());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].api_key_env"));
}

#[test]
fn llm_api_key_env_rejects_non_env_var_syntax() {
    // a literal-looking key must be refused — config stores references, not values
    let mut c = config_with_default_entry();
    c.llm[0].api_key_env = Some("sk-abcd1234".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].api_key_env"));
}

#[test]
fn llm_api_key_env_accepts_env_var_name() {
    let mut c = config_with_default_entry();
    c.llm[0].api_key_env = Some("OPENAI_API_KEY".into());
    assert!(c.validate().is_ok());

    // missing is fine (runtime falls back to vault / provider-specific env vars)
    let mut c = config_with_default_entry();
    c.llm[0].api_key_env = None;
    assert!(c.validate().is_ok());
}

#[test]
fn aggregates_multiple_errors() {
    let mut c = config_with_default_entry();
    c.llm[0].provider = String::new();
    c.llm[0].model = String::new();
    c.agent.max_iterations = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(errors.len() >= 3);
    assert!(has_field(&errors, "llm[0].provider"));
    assert!(has_field(&errors, "llm[0].model"));
    assert!(has_field(&errors, "agent.max_iterations"));
}

#[test]
fn set_at_path_updates_primitive() {
    let cfg = BayboConfig::default();
    let new = cfg
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("set");
    assert_eq!(new.agent.max_iterations, 7);
}

#[test]
fn set_at_path_accepts_slash_pointer() {
    let cfg = BayboConfig::default();
    let new = cfg
        .set_at_path("/agent/max_iterations", serde_json::json!(7))
        .expect("set");
    assert_eq!(new.agent.max_iterations, 7);
}

#[test]
fn set_at_path_rejects_empty_path() {
    let cfg = BayboConfig::default();
    let err = cfg
        .set_at_path("", serde_json::json!("x"))
        .expect_err("empty should fail");
    assert!(matches!(err, ConfigError::InvalidPath { .. }));
}

#[test]
fn set_at_path_rejects_value_that_fails_validation() {
    let cfg = BayboConfig::default();
    let err = cfg
        .set_at_path("agent.max_iterations", serde_json::json!(0))
        .expect_err("zero iterations invalid");
    assert!(matches!(err, ConfigError::Validation(_)));
}

#[test]
fn unset_at_path_resets_to_default() {
    let cfg = BayboConfig::default()
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("seed");
    let reset = cfg.unset_at_path("agent.max_iterations").expect("unset");
    assert_eq!(
        reset.agent.max_iterations,
        BayboConfig::default().agent.max_iterations
    );
}

#[test]
fn unset_at_path_rejects_empty() {
    let cfg = BayboConfig::default();
    let err = cfg.unset_at_path("").expect_err("empty should fail");
    assert!(matches!(err, ConfigError::InvalidPath { .. }));
}

#[test]
fn external_agents_disabled_by_default() {
    // Default config has no external agents enabled — even if a
    // claude/codex binary happens to be on PATH at boot, registration
    // must require an explicit operator opt-in.
    let c = BayboConfig::default();
    assert!(c.external_agents.enabled_kinds().is_empty());
    c.validate().expect("default config validates");
}

#[test]
fn external_agents_zero_or_one_enabled_default_optional() {
    // Zero enabled.
    let c = BayboConfig::default();
    c.validate()
        .expect("no external agents = no default needed");

    // Exactly one enabled, no default set: still fine.
    let mut c = BayboConfig::default();
    c.external_agents.claude.enabled = true;
    c.external_agents.claude.binary_path = Some("/usr/bin/claude".into());
    c.validate()
        .expect("single external agent = implicit default");
}

#[test]
fn external_agents_multiple_enabled_require_default() {
    let c = BayboConfig {
        external_agents: ExternalAgentsConfig {
            claude: ClaudeConfig {
                enabled: true,
                binary_path: Some("/usr/bin/claude".into()),
            },
            codex: CodexConfig {
                enabled: true,
                binary_path: Some("/usr/bin/codex".into()),
            },
            gemini: GeminiConfig::default(),
            default_external_agent: None,
        },
        ..BayboConfig::default()
    };
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "external_agents.default_external_agent"));
}

#[test]
fn external_agents_default_among_enabled_is_ok() {
    let c = BayboConfig {
        external_agents: ExternalAgentsConfig {
            claude: ClaudeConfig {
                enabled: true,
                binary_path: Some("/usr/bin/claude".into()),
            },
            codex: CodexConfig {
                enabled: true,
                binary_path: Some("/usr/bin/codex".into()),
            },
            gemini: GeminiConfig::default(),
            default_external_agent: Some(ExternalAgentKind::Claude),
        },
        ..BayboConfig::default()
    };
    c.validate().expect("default among enabled = OK");
}

#[test]
fn external_agents_binary_path_without_enabled_does_not_count() {
    // An operator who set binary_path but left enabled=false has not
    // actually opted in. Validation must NOT treat this as "configured"
    // for the multi-enabled default-required rule, since boot will
    // skip the kind entirely.
    let c = BayboConfig {
        external_agents: ExternalAgentsConfig {
            claude: ClaudeConfig {
                enabled: false,
                binary_path: Some("/usr/bin/claude".into()),
            },
            codex: CodexConfig {
                enabled: false,
                binary_path: Some("/usr/bin/codex".into()),
            },
            gemini: GeminiConfig::default(),
            default_external_agent: None,
        },
        ..BayboConfig::default()
    };
    c.validate()
        .expect("binary_path without enabled = not opted in");
    assert!(c.external_agents.enabled_kinds().is_empty());
}

#[test]
fn write_to_file_round_trips_through_load() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tmp = std::env::temp_dir().join(format!(
        "baybo-config-write-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cfg = BayboConfig::default()
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("seed");
    rt.block_on(cfg.write_to_file(&tmp)).expect("write");
    let loaded = rt
        .block_on(BayboConfig::load_from_file(&tmp))
        .expect("load");
    assert_eq!(loaded.agent.max_iterations, 7);
    std::fs::remove_file(&tmp).ok();
}
