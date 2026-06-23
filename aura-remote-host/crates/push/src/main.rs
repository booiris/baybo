//! The push role binary.
//!
//! Loads its config + APNs `.p8` from the environment (see [`PushConfig::from_env`])
//! and serves the `/notify` + `/register` routes. The only `aura-remote-host`
//! component that holds the crown-jewel key, so it's deployed on its own host.

use std::process::ExitCode;

use aura_remote_host_push::serve::{PushConfig, build_router};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aura-remote-host-push: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (config, p8_path) = PushConfig::from_env()?;
    let p8_pem = std::fs::read(&p8_path)
        .map_err(|e| format!("read .p8 at {}: {e}", p8_path.display()))?;
    let router = build_router(&config, &p8_pem)?;

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    eprintln!(
        "aura-remote-host-push: listening on {} (topic {})",
        config.bind_addr, config.topic,
    );
    axum::serve(listener, router).await?;
    Ok(())
}
