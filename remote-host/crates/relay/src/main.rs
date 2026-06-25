//! The relay role binary.
//!
//! Hosts the blind pairing rendezvous (`/pair/host/{code}` for admitted
//! gateways, `/pair/join/{code}` for apps). Config comes from the environment
//! ([`RelayConfig::from_env`]); it holds no key material and sees only opaque
//! SPAKE2 frames, so it can be deployed on its own host alongside the push role.

use std::process::ExitCode;

use remote_host_relay::serve::{RelayConfig, build_router};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("remote-host-relay: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = RelayConfig::from_env()?;
    let router = build_router(&config);
    let scheme = if config.tls.is_some() { "wss/https" } else { "ws/http" };
    eprintln!(
        "remote-host-relay: listening on {} ({scheme}, {} admitted gateways)",
        config.bind_addr,
        config.instance_keys.len(),
    );
    remote_host_serve::serve(&config.bind_addr, config.tls, router).await?;
    Ok(())
}
