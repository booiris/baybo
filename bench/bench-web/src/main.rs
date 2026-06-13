//! `bench-web` — local, read-only viewer for the bench harnesses'
//! on-disk `results/` + `trace/` artifacts. See `crate` docs in lib.rs.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "bench-web",
    about = "Read-only web viewer for aura bench results + agent traces"
)]
struct Args {
    /// Root directory holding the per-bench dirs (swe, terminal-bench-*,
    /// memory). Defaults to ./bench.
    #[arg(long, default_value = "bench")]
    root: PathBuf,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 7000)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("aura_bench_web=info")),
        )
        .init();

    let args = Args::parse();
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("bench root {} not found", args.root.display()))?;

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .with_context(|| format!("invalid host/port {}:{}", args.host, args.port))?;

    let app = aura_bench_web::api::router(root.clone());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    tracing::info!(%addr, root = %root.display(), "bench-web serving");
    println!("bench-web → http://{addr}  (root: {})", root.display());

    axum::serve(listener, app)
        .await
        .context("axum server error")?;
    Ok(())
}
