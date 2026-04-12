//! Contract tests for the mirror types in `aura-config`.
//!
//! These tests act as the drift detector between config-layer mirrors and the
//! domain types they reflect. If a domain enum grows a new variant without the
//! corresponding mirror update, the `From` impl's match loses exhaustiveness
//! and this crate fails to compile.
//!
//! Conversely, `TryFrom<Mirror> for Domain` returns `ConfigError::UnsupportedVariant`
//! when the mirror carries something the domain cannot (yet) represent. Today
//! the variant sets are fully aligned, so the error path is unreachable — but
//! the shape of the error is pinned so future additions on either side have a
//! defined failure mode.

use std::collections::HashMap;

use aura_config::{
    CapabilityConfig, ConfigError, McpTransportConfig, SecretAccessConfig, SecretRequirementConfig,
    TrustLevelConfig,
};

// ---------------------------------------------------------------------------
// TrustLevel ↔ TrustLevelConfig
// ---------------------------------------------------------------------------

fn trust_domain_to_mirror(d: &aura_registry::TrustLevel) -> TrustLevelConfig {
    match d {
        aura_registry::TrustLevel::Trusted => TrustLevelConfig::Trusted,
        aura_registry::TrustLevel::Installed => TrustLevelConfig::Installed,
        aura_registry::TrustLevel::Untrusted => TrustLevelConfig::Untrusted,
    }
}

fn trust_mirror_to_domain(m: TrustLevelConfig) -> Result<aura_registry::TrustLevel, ConfigError> {
    Ok(match m {
        TrustLevelConfig::Trusted => aura_registry::TrustLevel::Trusted,
        TrustLevelConfig::Installed => aura_registry::TrustLevel::Installed,
        TrustLevelConfig::Untrusted => aura_registry::TrustLevel::Untrusted,
    })
}

#[test]
fn trust_level_mirror_roundtrip() {
    let all = [
        aura_registry::TrustLevel::Trusted,
        aura_registry::TrustLevel::Installed,
        aura_registry::TrustLevel::Untrusted,
    ];
    for d in all {
        let mirror = trust_domain_to_mirror(&d);
        let back = trust_mirror_to_domain(mirror).expect("roundtrip");
        assert_eq!(d, back);
    }
}

// ---------------------------------------------------------------------------
// SecretAccess ↔ SecretAccessConfig
// ---------------------------------------------------------------------------

fn access_domain_to_mirror(d: aura_tools::SecretAccess) -> SecretAccessConfig {
    match d {
        aura_tools::SecretAccess::ReadOnly => SecretAccessConfig::ReadOnly,
        aura_tools::SecretAccess::ReadWrite => SecretAccessConfig::ReadWrite,
    }
}

fn access_mirror_to_domain(m: SecretAccessConfig) -> Result<aura_tools::SecretAccess, ConfigError> {
    Ok(match m {
        SecretAccessConfig::ReadOnly => aura_tools::SecretAccess::ReadOnly,
        SecretAccessConfig::ReadWrite => aura_tools::SecretAccess::ReadWrite,
    })
}

#[test]
fn secret_access_mirror_roundtrip() {
    for d in [
        aura_tools::SecretAccess::ReadOnly,
        aura_tools::SecretAccess::ReadWrite,
    ] {
        let back = access_mirror_to_domain(access_domain_to_mirror(d)).expect("roundtrip");
        assert_eq!(d, back);
    }
}

// ---------------------------------------------------------------------------
// ToolCapability ↔ CapabilityConfig
// ---------------------------------------------------------------------------

fn capability_domain_to_mirror(d: &aura_tools::ToolCapability) -> CapabilityConfig {
    match d {
        aura_tools::ToolCapability::ReadWorkspace => CapabilityConfig::ReadWorkspace,
        aura_tools::ToolCapability::WriteWorkspace => CapabilityConfig::WriteWorkspace,
        aura_tools::ToolCapability::Http(d) => CapabilityConfig::Http(d.clone()),
        aura_tools::ToolCapability::SpawnProcess => CapabilityConfig::SpawnProcess,
        aura_tools::ToolCapability::BrowserAutomation => CapabilityConfig::BrowserAutomation,
    }
}

fn capability_mirror_to_domain(
    m: CapabilityConfig,
) -> Result<aura_tools::ToolCapability, ConfigError> {
    Ok(match m {
        CapabilityConfig::ReadWorkspace => aura_tools::ToolCapability::ReadWorkspace,
        CapabilityConfig::WriteWorkspace => aura_tools::ToolCapability::WriteWorkspace,
        CapabilityConfig::Http(d) => aura_tools::ToolCapability::Http(d),
        CapabilityConfig::SpawnProcess => aura_tools::ToolCapability::SpawnProcess,
        CapabilityConfig::BrowserAutomation => aura_tools::ToolCapability::BrowserAutomation,
    })
}

#[test]
fn capability_mirror_roundtrip() {
    let samples = [
        aura_tools::ToolCapability::ReadWorkspace,
        aura_tools::ToolCapability::WriteWorkspace,
        aura_tools::ToolCapability::Http(vec!["example.com".into(), "api.example.org".into()]),
        aura_tools::ToolCapability::SpawnProcess,
        aura_tools::ToolCapability::BrowserAutomation,
    ];
    for d in &samples {
        let mirror = capability_domain_to_mirror(d);
        let back = capability_mirror_to_domain(mirror).expect("roundtrip");
        // ToolCapability doesn't derive PartialEq upstream, so compare via Debug.
        assert_eq!(format!("{d:?}"), format!("{back:?}"));
    }
}

// ---------------------------------------------------------------------------
// McpTransport ↔ McpTransportConfig
// ---------------------------------------------------------------------------

fn transport_domain_to_mirror(d: &aura_tools::mcp::McpTransport) -> McpTransportConfig {
    match d {
        aura_tools::mcp::McpTransport::Stdio { command, args, env } => McpTransportConfig::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
        },
        aura_tools::mcp::McpTransport::Http { url, headers } => McpTransportConfig::Http {
            url: url.clone(),
            headers: headers.clone(),
        },
    }
}

fn transport_mirror_to_domain(
    m: McpTransportConfig,
) -> Result<aura_tools::mcp::McpTransport, ConfigError> {
    Ok(match m {
        McpTransportConfig::Stdio { command, args, env } => {
            aura_tools::mcp::McpTransport::Stdio { command, args, env }
        }
        McpTransportConfig::Http { url, headers } => {
            aura_tools::mcp::McpTransport::Http { url, headers }
        }
    })
}

#[test]
fn mcp_transport_mirror_roundtrip() {
    let mut env = HashMap::new();
    env.insert("K".into(), "V".into());
    let mut headers = HashMap::new();
    headers.insert("Authorization".into(), "Bearer x".into());

    let samples = [
        aura_tools::mcp::McpTransport::Stdio {
            command: "server".into(),
            args: vec!["--flag".into()],
            env,
        },
        aura_tools::mcp::McpTransport::Http {
            url: "https://example.com".into(),
            headers,
        },
    ];
    for d in &samples {
        let mirror = transport_domain_to_mirror(d);
        let back = transport_mirror_to_domain(mirror).expect("roundtrip");
        assert_eq!(format!("{d:?}"), format!("{back:?}"));
    }
}

// ---------------------------------------------------------------------------
// SecretRequirement ↔ SecretRequirementConfig
//
// A composite test: exercises all three primitive mirror conversions plus the
// struct-level mapping used by consumers.
// ---------------------------------------------------------------------------

fn requirement_domain_to_mirror(d: &aura_tools::SecretRequirement) -> SecretRequirementConfig {
    SecretRequirementConfig {
        key: d.key.clone(),
        access: access_domain_to_mirror(d.access),
        required: d.required,
    }
}

fn requirement_mirror_to_domain(
    m: SecretRequirementConfig,
) -> Result<aura_tools::SecretRequirement, ConfigError> {
    Ok(aura_tools::SecretRequirement {
        key: m.key,
        access: access_mirror_to_domain(m.access)?,
        required: m.required,
    })
}

#[test]
fn secret_requirement_mirror_roundtrip() {
    let d = aura_tools::SecretRequirement {
        key: "OPENAI_API_KEY".into(),
        access: aura_tools::SecretAccess::ReadWrite,
        required: false,
    };
    let back = requirement_mirror_to_domain(requirement_domain_to_mirror(&d)).expect("roundtrip");
    assert_eq!(format!("{d:?}"), format!("{back:?}"));
}

// ---------------------------------------------------------------------------
// UnsupportedVariant error shape
//
// Today no mirror carries a variant the domain lacks, so the error path is
// unreachable from these tests. We still pin the error's shape — if a future
// mirror addition needs the escape hatch, it can rely on this format.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_variant_error_shape_is_stable() {
    let e = ConfigError::UnsupportedVariant {
        ty: "MyEnum".into(),
        variant: "NewThing".into(),
    };
    let msg = e.to_string();
    assert!(msg.contains("MyEnum"), "missing type name: {msg}");
    assert!(msg.contains("NewThing"), "missing variant name: {msg}");
}
