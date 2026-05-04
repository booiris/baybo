use aura_config::{
    AuraConfig, ConfigError, DiscordChannelConfig, HttpChannelConfig, LlmEntry,
    TelegramChannelConfig,
};

fn has_field(errors: &[aura_config::ValidationError], field: &str) -> bool {
    errors.iter().any(|e| e.field == field)
}

fn unwrap_validation(err: ConfigError) -> Vec<aura_config::ValidationError> {
    match err {
        ConfigError::Validation(v) => v,
        other => panic!("expected Validation error, got {other:?}"),
    }
}

#[test]
fn default_config_is_valid() {
    let config = AuraConfig::default();
    config.validate().expect("default config should validate");
}

#[test]
fn empty_json_uses_defaults() {
    let config = AuraConfig::load_from_str("{}").expect("empty object should parse");
    assert_eq!(config, AuraConfig::default());
}

#[test]
fn invalid_json_returns_parse_error() {
    let err = AuraConfig::load_from_str("not json").unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

fn entry(name: &str) -> LlmEntry {
    LlmEntry {
        name: name.into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key_env: None,
        base_url: None,
        supports_vision: None,
        reasoning_effort: None,
    }
}

fn config_with_default_entry() -> AuraConfig {
    AuraConfig {
        llm: vec![entry("openai")],
        default_llm: "openai".into(),
        ..AuraConfig::default()
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

#[test]
fn bad_base_url_fails_validation() {
    let mut c = config_with_default_entry();
    c.llm[0].base_url = Some("ftp://x".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm[0].base_url"));
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
    let c = AuraConfig::default();
    assert!(c.llm.is_empty());
    assert!(c.default_llm.is_empty());
    assert!(c.validate().is_ok());
}

#[test]
fn default_llm_required_when_entries_exist() {
    let c = AuraConfig {
        llm: vec![entry("openai")],
        default_llm: String::new(),
        ..AuraConfig::default()
    };
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "default-llm"));
}

#[test]
fn default_llm_must_reference_existing_entry() {
    let c = AuraConfig {
        llm: vec![entry("openai")],
        default_llm: "missing".into(),
        ..AuraConfig::default()
    };
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "default-llm"));
}

#[test]
fn compression_threshold_bounds() {
    // zero is invalid
    let mut c = AuraConfig::default();
    c.agent.context.compression_threshold = 0.0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.context.compression_threshold"));

    // above 1 is invalid
    let mut c = AuraConfig::default();
    c.agent.context.compression_threshold = 1.5;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.context.compression_threshold"));

    // exactly 1 is valid
    let mut c = AuraConfig::default();
    c.agent.context.compression_threshold = 1.0;
    assert!(c.validate().is_ok());
}

#[test]
fn max_iterations_bounds() {
    let mut c = AuraConfig::default();
    c.agent.max_iterations = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.max_iterations"));

    let mut c = AuraConfig::default();
    c.agent.max_iterations = 1001;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.max_iterations"));
}

#[test]
fn tool_timeout_must_be_at_least_100ms() {
    let mut c = AuraConfig::default();
    c.agent.default_tool_timeout_ms = 50;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "agent.default_tool_timeout_ms"));
}

#[test]
fn session_timeout_must_be_positive() {
    let mut c = AuraConfig::default();
    c.session.timeout_minutes = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "session.timeout_minutes"));
}

#[test]
fn channel_buffer_bounds() {
    let mut c = AuraConfig::default();
    c.channels.message_buffer_size = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.message_buffer_size"));

    let mut c = AuraConfig::default();
    c.channels.message_buffer_size = 100_000;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.message_buffer_size"));
}

#[test]
fn http_channel_requires_bind_address_and_port() {
    let mut c = AuraConfig::default();
    c.channels.http = Some(HttpChannelConfig {
        enabled: true,
        bind_address: String::new(),
        port: 0,
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.http.bind_address"));
    assert!(has_field(&errors, "channels.http.port"));
}

#[test]
fn spending_limits_must_be_positive() {
    let mut c = AuraConfig::default();
    c.cost.spending_limits.user_daily_usd = Some(-1.0);
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.spending_limits.user_daily_usd"));
}

#[test]
fn daily_cannot_exceed_monthly_spend() {
    let mut c = AuraConfig::default();
    c.cost.spending_limits.user_daily_usd = Some(100.0);
    c.cost.spending_limits.user_monthly_usd = Some(50.0);
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.spending_limits.user_daily_usd"));
}

#[test]
fn rate_limit_fields_must_be_positive() {
    let mut c = AuraConfig::default();
    c.cost.rate_limit.max_requests = 0;
    c.cost.rate_limit.window_secs = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "cost.rate_limit.max_requests"));
    assert!(has_field(&errors, "cost.rate_limit.window_secs"));
}

#[test]
fn full_roundtrip_via_json() {
    let config = AuraConfig::default();
    let json = serde_json::to_string(&config).expect("serialize");
    let parsed = AuraConfig::load_from_str(&json).expect("reparse");
    assert_eq!(parsed, config);
}

#[test]
fn load_from_file_reads_and_parses() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join("aura-config-test.json");
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
    let config = rt.block_on(AuraConfig::load_from_file(&tmp))?;
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
        .block_on(AuraConfig::load_from_file(std::path::Path::new(
            "/nonexistent/path/aura-config-missing.json",
        )))
        .unwrap_err();
    assert!(matches!(err, ConfigError::FileRead { .. }));
}

#[test]
fn http_channel_disabled_flag_is_rejected() {
    let mut c = AuraConfig::default();
    c.channels.http = Some(HttpChannelConfig {
        enabled: false,
        bind_address: "127.0.0.1".into(),
        port: 8080,
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.http"));
}

#[test]
fn telegram_channel_disabled_flag_is_rejected() {
    let mut c = AuraConfig::default();
    c.channels.telegram = Some(TelegramChannelConfig {
        enabled: false,
        bot_token_env: "TG_TOKEN".into(),
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.telegram"));
}

#[test]
fn discord_channel_disabled_flag_is_rejected() {
    let mut c = AuraConfig::default();
    c.channels.discord = Some(DiscordChannelConfig {
        enabled: false,
        bot_token_env: "DC_TOKEN".into(),
    });
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "channels.discord"));
}

#[test]
fn encryption_key_requires_a_source() {
    let mut c = AuraConfig::default();
    c.security.encryption_key_file = None;
    c.security.encryption_key_env = String::new();
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "security.encryption_key"));

    // a file path alone is sufficient
    let mut c = AuraConfig::default();
    c.security.encryption_key_file = Some("/tmp/key".into());
    c.security.encryption_key_env = String::new();
    assert!(c.validate().is_ok());

    // env var name alone is sufficient (this is also the default)
    let mut c = AuraConfig::default();
    c.security.encryption_key_file = None;
    c.security.encryption_key_env = "AURA_KEY".into();
    assert!(c.validate().is_ok());
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
    c.session.timeout_minutes = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(errors.len() >= 3);
    assert!(has_field(&errors, "llm[0].provider"));
    assert!(has_field(&errors, "llm[0].model"));
    assert!(has_field(&errors, "session.timeout_minutes"));
}

#[test]
fn set_at_path_updates_primitive() {
    let cfg = AuraConfig::default();
    let new = cfg
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("set");
    assert_eq!(new.agent.max_iterations, 7);
}

#[test]
fn set_at_path_accepts_slash_pointer() {
    let cfg = AuraConfig::default();
    let new = cfg
        .set_at_path("/agent/max_iterations", serde_json::json!(7))
        .expect("set");
    assert_eq!(new.agent.max_iterations, 7);
}

#[test]
fn set_at_path_rejects_empty_path() {
    let cfg = AuraConfig::default();
    let err = cfg
        .set_at_path("", serde_json::json!("x"))
        .expect_err("empty should fail");
    assert!(matches!(err, ConfigError::InvalidPath { .. }));
}

#[test]
fn set_at_path_rejects_value_that_fails_validation() {
    let cfg = AuraConfig::default();
    let err = cfg
        .set_at_path("agent.max_iterations", serde_json::json!(0))
        .expect_err("zero iterations invalid");
    assert!(matches!(err, ConfigError::Validation(_)));
}

#[test]
fn unset_at_path_resets_to_default() {
    let cfg = AuraConfig::default()
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("seed");
    let reset = cfg.unset_at_path("agent.max_iterations").expect("unset");
    assert_eq!(
        reset.agent.max_iterations,
        AuraConfig::default().agent.max_iterations
    );
}

#[test]
fn unset_at_path_rejects_empty() {
    let cfg = AuraConfig::default();
    let err = cfg.unset_at_path("").expect_err("empty should fail");
    assert!(matches!(err, ConfigError::InvalidPath { .. }));
}

#[test]
fn write_to_file_round_trips_through_load() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tmp = std::env::temp_dir().join(format!(
        "aura-config-write-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cfg = AuraConfig::default()
        .set_at_path("agent.max_iterations", serde_json::json!(7))
        .expect("seed");
    rt.block_on(cfg.write_to_file(&tmp)).expect("write");
    let loaded = rt.block_on(AuraConfig::load_from_file(&tmp)).expect("load");
    assert_eq!(loaded.agent.max_iterations, 7);
    std::fs::remove_file(&tmp).ok();
}
