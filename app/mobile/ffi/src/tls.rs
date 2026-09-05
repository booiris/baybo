//! The one place the HTTP legs choose a TLS trust anchor.
//!
//! Two of the three legs never come here: relay pairing, relay chat and the
//! direct chat leg all dial through `tokio_tungstenite::connect_async`, whose
//! `rustls-tls-webpki-roots` feature bakes Mozilla's root program into the
//! binary on every platform. Only the direct leg's REST/blob client is built
//! from `reqwest`, and that is what this module configures.
//!
//! **Android would otherwise panic on its first request.** `reqwest`'s
//! `rustls-no-provider` feature pulls in `rustls-platform-verifier`, which on
//! Android is a JNI shim over the system trust manager: it needs
//! `rustls_platform_verifier::android::init_with_env` plus a bundled Kotlin
//! component before any handshake, and without them the verifier `expect`s its
//! way to a process abort — after `Client::builder().build()` has already
//! returned `Ok`. So Android gets an explicitly preconfigured rustls instead.
//!
//! The cost is a documented asymmetry, and a smaller one than it looks: the
//! direct REST half on iOS trusts the platform store (user-installed CAs
//! included) while Android trusts the public roots only. A self-hosted gateway
//! behind a private CA is *already* unusable on both, because its chat leg is a
//! `wss://` dial through the baked-in roots above — this only makes the REST
//! half agree with the leg that decides whether the app works at all. Teaching
//! every leg to use the platform store is the follow-up (option A in
//! `docs/todo/android-companion.md`), and it belongs to all four call sites at
//! once, not to this one.

/// A `reqwest` client builder with the platform's trust anchors already chosen.
#[cfg(not(target_os = "android"))]
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

#[cfg(target_os = "android")]
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // `with_provider` rather than `builder()`: the crate is compiled with no
    // default provider (see the workspace manifest), and the process installs
    // ring in `BayboClient::new`. Naming it here keeps this independent of the
    // order those two happen in.
    let config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .expect("ring supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();

    // NOT `use_preconfigured_tls`, which reqwest 0.13 documents as a deprecated
    // shim over this. Both skip the platform-verifier path; only this one will
    // still be here after the attribute lands and the `-D warnings` gate fires.
    reqwest::Client::builder().tls_backend_preconfigured(config)
}
