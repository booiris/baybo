use std::env;

/// FALLBACK suffix for the shared keychain access group. An iOS app resolves its
/// group at RUNTIME from its own `BayboKeychainAccessGroup` Info key (see
/// `keychain::imp::access_group`) — it has to, because one xcframework serves
/// every configuration and a local build carries a different bundle id. What is
/// baked in here answers embedders with no USABLE Info key: host tests, and any
/// build where `$(AppIdentifierPrefix)` never expanded (it is signing that
/// expands it, so an unsigned build leaves a bare leading dot).
/// `scripts/verify-nse.sh` is NOT such a case and must not be read as one — it
/// patches `BayboKeychainAccessGroup` in both plists precisely so the RUNTIME
/// path is the one under test. Delete that patch and this fallback takes over
/// on one side only, which is a silent NSE decrypt failure.
const KEYCHAIN_ACCESS_GROUP_SUFFIX: &str = "com.baybo.app";

fn main() {
    emit_ios_keychain_access_group();
}

fn emit_ios_keychain_access_group() {
    for key in [
        "BAYBO_IOS_KEYCHAIN_ACCESS_GROUP",
        "BAYBO_IOS_KEYCHAIN_GROUP_SUFFIX",
        "APP_IDENTIFIER_PREFIX",
        "AppIdentifierPrefix",
        "DEVELOPMENT_TEAM",
        "BAYBO_IOS_TEAM_ID",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    if let Some(access_group) = ios_keychain_access_group() {
        println!("cargo:rustc-env=BAYBO_IOS_KEYCHAIN_ACCESS_GROUP={access_group}");
    }
}

fn ios_keychain_access_group() -> Option<String> {
    if let Some(access_group) = non_empty_env("BAYBO_IOS_KEYCHAIN_ACCESS_GROUP") {
        return Some(access_group);
    }

    let suffix = non_empty_env("BAYBO_IOS_KEYCHAIN_GROUP_SUFFIX")
        .unwrap_or_else(|| KEYCHAIN_ACCESS_GROUP_SUFFIX.to_string());

    // The App-Identifier prefix is the signing team; during an iOS build xcodebuild
    // exposes it as `DEVELOPMENT_TEAM` / `AppIdentifierPrefix` (the team is hardcoded
    // in the Xcode project). `BAYBO_IOS_TEAM_ID` stays as a manual override.
    non_empty_env("APP_IDENTIFIER_PREFIX")
        .or_else(|| non_empty_env("AppIdentifierPrefix"))
        .or_else(|| non_empty_env("DEVELOPMENT_TEAM"))
        .or_else(|| non_empty_env("BAYBO_IOS_TEAM_ID"))
        .map(|prefix| join_identifier_prefix(&prefix, &suffix))
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.contains("$("))
}

fn join_identifier_prefix(prefix: &str, suffix: &str) -> String {
    let prefix = prefix.trim();
    let separator = if prefix.ends_with('.') { "" } else { "." };
    format!("{prefix}{separator}{suffix}")
}
