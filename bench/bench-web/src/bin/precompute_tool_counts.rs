//! Walk a bench root and (re)write each trace's `<trace>.tools.json`
//! sidecar — the per-tool call counts the bench viewer's list reads
//! cheaply. Run for a one-off backfill or after a bench finishes (e.g.
//! from `run.sh` / `consolidate.sh`). See [`aura_bench_web::precompute`].

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "precompute-tool-counts",
    about = "Write <trace>.tools.json sidecars (per-tool call counts) for the bench viewer"
)]
struct Args {
    /// Bench root holding the per-bench dirs (swe, terminal-bench-*,
    /// memory). Defaults to ./bench.
    #[arg(long, default_value = "bench")]
    root: PathBuf,
}

fn main() -> anyhow::Result<()> {
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
    let n = aura_bench_web::precompute::write_tool_count_sidecars(&root)?;
    println!("wrote {n} tool-count sidecar(s) under {}", root.display());
    Ok(())
}
