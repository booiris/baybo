use aura_config::{
    AuraConfig, CapabilityConfig, ConfigError, DiscordChannelConfig, HttpChannelConfig,
    McpServerEntry, McpTransportConfig, TelegramChannelConfig, TrustLevelConfig,
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

#[test]
fn empty_provider_fails_validation() {
    let json = r#"{"llm":{"provider":"","model":"m"}}"#;
    let err = AuraConfig::load_from_str(json).unwrap_err();
    let errors = unwrap_validation(err);
    assert!(has_field(&errors, "llm.provider"));
}

#[test]
fn empty_model_fails_validation() {
    let json = r#"{"llm":{"provider":"openai","model":""}}"#;
    let err = AuraConfig::load_from_str(json).unwrap_err();
    let errors = unwrap_validation(err);
    assert!(has_field(&errors, "llm.model"));
}

#[test]
fn bad_base_url_fails_validation() {
    let json = r#"{"llm":{"provider":"openai","model":"m","base_url":"ftp://x"}}"#;
    let err = AuraConfig::load_from_str(json).unwrap_err();
    let errors = unwrap_validation(err);
    assert!(has_field(&errors, "llm.base_url"));
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
fn sandbox_memory_minimum() {
    let mut c = AuraConfig::default();
    c.sandbox.wasm.max_memory_bytes = 1024;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "sandbox.wasm.max_memory_bytes"));
}

#[test]
fn mcp_server_name_must_be_unique() {
    let mut c = AuraConfig::default();
    c.tools.mcp_servers = vec![
        McpServerEntry {
            name: "dup".into(),
            transport: McpTransportConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: Default::default(),
            },
            secret_requirements: vec![],
            trust_level: TrustLevelConfig::Installed,
            capabilities: vec![],
        },
        McpServerEntry {
            name: "dup".into(),
            transport: McpTransportConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: Default::default(),
            },
            secret_requirements: vec![],
            trust_level: TrustLevelConfig::Installed,
            capabilities: vec![],
        },
    ];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "tools.mcp_servers[1].name"));
}

#[test]
fn mcp_http_requires_url_scheme() {
    let mut c = AuraConfig::default();
    c.tools.mcp_servers = vec![McpServerEntry {
        name: "srv".into(),
        transport: McpTransportConfig::Http {
            url: "example.com".into(),
            headers: Default::default(),
        },
        secret_requirements: vec![],
        trust_level: TrustLevelConfig::Installed,
        capabilities: vec![],
    }];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "tools.mcp_servers[0].transport.url"));
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
fn trace_snapshot_interval_required_when_enabled() {
    let mut c = AuraConfig::default();
    c.trace.auto_snapshot = true;
    c.trace.snapshot_interval = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "trace.snapshot_interval"));

    // disabled snapshot allows zero interval
    let mut c = AuraConfig::default();
    c.trace.auto_snapshot = false;
    c.trace.snapshot_interval = 0;
    assert!(c.validate().is_ok());
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
        r#"{"llm":{"provider":"anthropic","model":"claude-sonnet-4-6"}}"#,
    )?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let config = rt.block_on(AuraConfig::load_from_file(&tmp))?;
    assert_eq!(config.llm.provider, "anthropic");
    assert_eq!(config.llm.model, "claude-sonnet-4-6");
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

fn mcp_http_server(name: &str, url: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: Default::default(),
        },
        secret_requirements: vec![],
        trust_level: TrustLevelConfig::Trusted,
        capabilities: vec![],
    }
}

#[test]
fn mcp_http_host_must_be_covered_by_allowlist() {
    let mut c = AuraConfig::default();
    c.tools.mcp_servers = vec![mcp_http_server("srv", "https://api.example.com/rpc")];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "tools.mcp_servers[0].transport.url"));
}

#[test]
fn mcp_http_host_suffix_match_accepts_subdomain() {
    let mut c = AuraConfig::default();
    c.sandbox.network.allowed_domains = vec!["example.com".into()];
    c.tools.mcp_servers = vec![mcp_http_server("srv", "https://api.example.com:8443/rpc")];
    assert!(c.validate().is_ok());
}

#[test]
fn mcp_http_host_suffix_match_rejects_non_subdomain() {
    let mut c = AuraConfig::default();
    c.sandbox.network.allowed_domains = vec!["example.com".into()];
    // "notexample.com" shares a suffix substring but is not a subdomain
    c.tools.mcp_servers = vec![mcp_http_server("srv", "https://notexample.com/rpc")];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "tools.mcp_servers[0].transport.url"));
}

#[test]
fn mcp_http_loopback_allowed_when_flag_set() {
    let mut c = AuraConfig::default();
    c.sandbox.network.allow_loopback = true;
    c.tools.mcp_servers = vec![
        mcp_http_server("srv1", "http://localhost:8080/"),
        mcp_http_server("srv2", "http://127.0.0.1/"),
        mcp_http_server("srv3", "http://[::1]:9090/"),
    ];
    assert!(c.validate().is_ok());
}

#[test]
fn mcp_http_loopback_rejected_when_flag_unset() {
    let mut c = AuraConfig::default();
    c.sandbox.network.allow_loopback = false;
    c.tools.mcp_servers = vec![mcp_http_server("srv", "http://127.0.0.1:9090/")];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "tools.mcp_servers[0].transport.url"));
}

#[test]
fn installed_trust_level_forbids_destructive_capabilities() {
    let mut c = AuraConfig::default();
    c.tools.mcp_servers = vec![McpServerEntry {
        name: "srv".into(),
        transport: McpTransportConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: Default::default(),
        },
        secret_requirements: vec![],
        trust_level: TrustLevelConfig::Installed,
        capabilities: vec![
            CapabilityConfig::WriteWorkspace,
            CapabilityConfig::SpawnProcess,
        ],
    }];
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.field == "tools.mcp_servers[0].capabilities")
            .count(),
        2
    );
}

#[test]
fn trusted_level_allows_destructive_capabilities() {
    let mut c = AuraConfig::default();
    c.tools.mcp_servers = vec![McpServerEntry {
        name: "srv".into(),
        transport: McpTransportConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: Default::default(),
        },
        secret_requirements: vec![],
        trust_level: TrustLevelConfig::Trusted,
        capabilities: vec![
            CapabilityConfig::WriteWorkspace,
            CapabilityConfig::SpawnProcess,
        ],
    }];
    assert!(c.validate().is_ok());
}

#[test]
fn llm_api_key_env_rejects_empty_string() {
    let mut c = AuraConfig::default();
    c.llm.api_key_env = Some(String::new());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm.api_key_env"));
}

#[test]
fn llm_api_key_env_rejects_non_env_var_syntax() {
    // a literal-looking key must be refused — config stores references, not values
    let mut c = AuraConfig::default();
    c.llm.api_key_env = Some("sk-abcd1234".into());
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(has_field(&errors, "llm.api_key_env"));
}

#[test]
fn llm_api_key_env_accepts_env_var_name() {
    let mut c = AuraConfig::default();
    c.llm.api_key_env = Some("OPENAI_API_KEY".into());
    assert!(c.validate().is_ok());

    // missing is fine (runtime falls back to provider-specific env vars)
    let mut c = AuraConfig::default();
    c.llm.api_key_env = None;
    assert!(c.validate().is_ok());
}

#[test]
fn aggregates_multiple_errors() {
    let mut c = AuraConfig::default();
    c.llm.provider = String::new();
    c.llm.model = String::new();
    c.session.timeout_minutes = 0;
    let errors = unwrap_validation(c.validate().unwrap_err());
    assert!(errors.len() >= 3);
    assert!(has_field(&errors, "llm.provider"));
    assert!(has_field(&errors, "llm.model"));
    assert!(has_field(&errors, "session.timeout_minutes"));
}

#[test]
fn set_at_path_updates_primitive() {
    let cfg = AuraConfig::default();
    let new = cfg
        .set_at_path("llm.model", serde_json::json!("gpt-5"))
        .expect("set");
    assert_eq!(new.llm.model, "gpt-5");
}

#[test]
fn set_at_path_accepts_slash_pointer() {
    let cfg = AuraConfig::default();
    let new = cfg
        .set_at_path("/llm/model", serde_json::json!("gpt-5"))
        .expect("set");
    assert_eq!(new.llm.model, "gpt-5");
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
        .set_at_path("llm.provider", serde_json::json!(""))
        .expect_err("empty provider invalid");
    assert!(matches!(err, ConfigError::Validation(_)));
}

#[test]
fn unset_at_path_resets_to_default() {
    let cfg = AuraConfig::default()
        .set_at_path("llm.model", serde_json::json!("gpt-5"))
        .expect("seed");
    let reset = cfg.unset_at_path("llm.model").expect("unset");
    assert_eq!(reset.llm.model, AuraConfig::default().llm.model);
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
        .set_at_path("llm.model", serde_json::json!("gpt-5"))
        .expect("seed");
    rt.block_on(cfg.write_to_file(&tmp)).expect("write");
    let loaded = rt.block_on(AuraConfig::load_from_file(&tmp)).expect("load");
    assert_eq!(loaded.llm.model, "gpt-5");
    std::fs::remove_file(&tmp).ok();
}
