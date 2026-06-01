//! Phase 1 of the memory benchmark: populate a backend through its concrete
//! methods (per-turn writes, then commit/extraction), poll extraction to true
//! completion, and emit a manifest the `run` bin consumes. Conversations come
//! from the selected `--testset`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use aura_bench_memory::backend::{Backend, BackendOpts, SettleOpts, build_backend, settle_probe};
use aura_bench_memory::testset::TestSetKind;
use aura_bench_memory::{Manifest, scope_user_id};
use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Arm {
    Mem0,
    Openviking,
}

impl From<Arm> for Backend {
    fn from(arm: Arm) -> Self {
        match arm {
            Arm::Mem0 => Backend::Mem0,
            Arm::Openviking => Backend::OpenViking,
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "memory benchmark — ingest a conversation set into a memory backend")]
struct Args {
    /// Backend to populate. (noop/oracle arms don't ingest.)
    #[arg(long, value_enum)]
    arm: Arm,

    /// Which test set to ingest.
    #[arg(long, value_enum, default_value = "locomo")]
    testset: TestSetKind,

    /// Path to the dataset file for the chosen test set.
    #[arg(long)]
    dataset: PathBuf,

    /// How many conversations to ingest, from the start of the dataset.
    #[arg(long, default_value_t = 1)]
    conversations: usize,

    /// Run id; scopes every key. Defaults to a fresh timestamp+random.
    #[arg(long)]
    run_id: Option<String>,

    /// Manifest output path (default: manifest-{arm}-{run_id}.json).
    #[arg(long)]
    out: Option<PathBuf>,

    /// top_k for recall (used by the settle probe and later by QA).
    #[arg(long, default_value_t = 10)]
    top_k: usize,

    /// Override the mem0 base URL (e.g. a self-hosted gateway).
    #[arg(long)]
    mem0_base_url: Option<String>,

    /// OpenViking endpoint (or set OPENVIKING_ENDPOINT).
    #[arg(long)]
    openviking_endpoint: Option<String>,

    /// OpenViking account header.
    #[arg(long)]
    openviking_account: Option<String>,

    #[arg(long, default_value_t = 3600)]
    settle_timeout_secs: u64,
    #[arg(long, default_value_t = 2)]
    settle_interval_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    let run_id = args.run_id.clone().unwrap_or_else(default_run_id);
    let arm: Backend = args.arm.into();
    let testset_name = args.testset.name();
    let samples = args.testset.test_set().load(&args.dataset)?;
    let n = args.conversations.min(samples.len());
    tracing::info!(testset = testset_name, arm = arm.as_str(), %run_id, conversations = n, "ingest start");

    let handle = build_backend(
        arm,
        &BackendOpts {
            top_k: args.top_k,
            mem0_base_url: args.mem0_base_url.clone(),
            openviking_endpoint: args.openviking_endpoint.clone(),
            openviking_account: args.openviking_account.clone(),
        },
    )?;
    let settle = SettleOpts {
        timeout: Duration::from_secs(args.settle_timeout_secs),
        interval: Duration::from_secs(args.settle_interval_secs),
    };

    let mut conversations = Vec::new();
    let mut all_settled = true;
    for (idx, sample) in samples.iter().take(n).enumerate() {
        let user_id = scope_user_id(testset_name, &run_id, arm.as_str(), idx);
        tracing::info!(conv = idx, %user_id, "ingesting");
        let mut scope = handle.ingest_conversation(&user_id, idx, sample).await?;

        let probe = settle_probe(&sample.conversation);
        let outcome = handle
            .settle(&user_id, &scope.session_ids, &probe, &settle)
            .await?;

        scope.memories_stored = outcome.count;
        if !outcome.stabilized || outcome.count == 0 {
            all_settled = false;
            tracing::warn!(
                conv = idx,
                count = outcome.count,
                stabilized = outcome.stabilized,
                "extraction did not settle cleanly before timeout"
            );
        }
        tracing::info!(
            conv = idx,
            turn_pairs = scope.turn_pairs,
            memories = outcome.count,
            "ingested"
        );
        conversations.push(scope);
    }

    let manifest = Manifest {
        run_id: run_id.clone(),
        testset: testset_name.to_string(),
        dataset: args.dataset.display().to_string(),
        arm: arm.as_str().to_string(),
        settled: all_settled,
        conversations,
    };
    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("manifest-{}-{run_id}.json", arm.as_str())));
    std::fs::write(&out, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write manifest {}", out.display()))?;
    tracing::info!(path = %out.display(), "manifest written");
    println!("{}", out.display());
    Ok(())
}

fn default_run_id() -> String {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{ts}-{}", &suffix[..6])
}
