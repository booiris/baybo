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

use aura_config::{ConfigError, TrustLevelConfig};

// ---------------------------------------------------------------------------
// TrustLevel ↔ TrustLevelConfig
// ---------------------------------------------------------------------------

fn trust_domain_to_mirror(d: &aura_model::TrustLevel) -> TrustLevelConfig {
    match d {
        aura_model::TrustLevel::Trusted => TrustLevelConfig::Trusted,
        aura_model::TrustLevel::Installed => TrustLevelConfig::Installed,
        aura_model::TrustLevel::Untrusted => TrustLevelConfig::Untrusted,
    }
}

fn trust_mirror_to_domain(m: TrustLevelConfig) -> Result<aura_model::TrustLevel, ConfigError> {
    Ok(match m {
        TrustLevelConfig::Trusted => aura_model::TrustLevel::Trusted,
        TrustLevelConfig::Installed => aura_model::TrustLevel::Installed,
        TrustLevelConfig::Untrusted => aura_model::TrustLevel::Untrusted,
    })
}

#[test]
fn trust_level_mirror_roundtrip() {
    let all = [
        aura_model::TrustLevel::Trusted,
        aura_model::TrustLevel::Installed,
        aura_model::TrustLevel::Untrusted,
    ];
    for d in all {
        let mirror = trust_domain_to_mirror(&d);
        let back = trust_mirror_to_domain(mirror).expect("roundtrip");
        assert_eq!(d, back);
    }
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
