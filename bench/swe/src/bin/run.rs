//! Run the SWE-bench benchmark for one arm and write a report.
//!
//! - **agent** — run aura inside each instance's eval image, capture `git diff`,
//!   grade the predictions with the `swebench` harness. Needs the musl `aura`
//!   (static-musl, `--features bench-bash`), a model key, and pre-built images.
//! - **oracle** — grade `--predictions_path gold` (the ceiling). No aura, no keys.
//! - **noop** — grade empty patches (the floor). No aura, no keys.
//!
//! One arm per invocation; run once per arm and compare the JSONs. Grading
//! always runs in the official Docker images, so oracle ≈100% / noop 0% validate
//! the whole pipeline offline before the agent arm spends anything.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use aura_bench_swe::agent::{self, AgentModel, RunOpts};
use aura_bench_swe::grader::{self, GraderConfig, Predictions};
use aura_bench_swe::report::{InstanceResult, ReportMeta, aggregate, print_table};
use aura_bench_swe::{
    AURA_API_KEY_ENV, SweInstance, arm_model_name, default_run_id, load_instances, parse_model,
    prediction_line, predictions_jsonl,
};
use clap::{Parser, ValueEnum};
use futures::{StreamExt, stream};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Arm {
    /// Run the real aura agent in each eval image (the measurement).
    Agent,
    /// Grade the gold patches (ceiling). No aura, no keys.
    Oracle,
    /// Grade empty patches (floor). No aura, no keys.
    Noop,
}

impl Arm {
    fn as_str(self) -> &'static str {
        match self {
            Arm::Agent => "agent",
            Arm::Oracle => "oracle",
            Arm::Noop => "noop",
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "SWE-bench — run one arm against the official swebench grader")]
struct Args {
    /// Which arm to evaluate.
    #[arg(long, value_enum)]
    arm: Arm,

    /// HuggingFace dataset name passed to the grader (and exported by swe_export.py).
    #[arg(long, default_value = "princeton-nlp/SWE-bench_Lite")]
    dataset_name: String,

    /// Dataset split.
    #[arg(long, default_value = "test")]
    split: String,

    /// Docker image namespace. "swebench" => prebuilt Hub images (pulled on
    /// demand; fast, default); "none" => local-build images (need prepare_images).
    /// MUST match the --namespace passed to swe_export.py.
    #[arg(long, default_value = "swebench")]
    namespace: String,

    /// `instances.json` from `swe_export.py` (instance metadata + image keys).
    #[arg(long)]
    instances_json: PathBuf,

    /// Restrict to these instance ids (default: every instance in the file).
    #[arg(long, num_args = 0..)]
    instance_ids: Vec<String>,

    /// Host path of the `aura` binary built `--features bench-bash` (static-musl)
    /// (musl-static recommended). Required for the agent arm.
    #[arg(long)]
    aura_bin: Option<PathBuf>,

    /// Host path of a static-musl `rg` (ripgrep), bundled into each eval container
    /// so aura's `Grep` tool works (the SWE-bench images don't ship ripgrep).
    /// Required for the agent arm.
    #[arg(long)]
    rg_bin: Option<PathBuf>,

    /// Agent LLM as `<provider>/<model>` (e.g. `deepseek/deepseek-v4-flash`,
    /// `openai/gpt-4o`, `anthropic/claude-3-5-sonnet`). No `provider/` prefix
    /// assumes the `deepseek` provider. (agent arm)
    #[arg(long, default_value = "deepseek/deepseek-v4-flash")]
    model: String,
    /// Custom LLM base URL (proxy / self-hosted / gateway). Empty => the
    /// provider's built-in endpoint.
    #[arg(long)]
    base_url: Option<String>,

    /// Base dir for per-run transcripts + call-tree traces
    /// (`<dir>/<run_id>/<arm>/<instance>.{messages,trace}.json`). (agent arm)
    #[arg(long, default_value = "trace")]
    trace_dir: PathBuf,
    /// Disable transcript/trace export (default: export every agent run).
    #[arg(long)]
    no_trace: bool,

    /// Eval containers run concurrently (agent arm).
    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// `--max_workers` for the grader harness.
    #[arg(long, default_value_t = 4)]
    max_workers: usize,

    /// Per-instance `aura prompt` timeout, seconds (agent arm).
    #[arg(long, default_value_t = 1800)]
    prompt_timeout: u64,

    /// Stable id for this run (report files, sessions). Default: timestamp+rand.
    #[arg(long)]
    run_id: Option<String>,

    /// Directory for the final results JSON report only.
    #[arg(long, default_value = "results")]
    results_dir: PathBuf,

    /// Directory for the run's working artifacts — predictions, the swebench
    /// harness report, and its per-instance logs (kept out of `results/`).
    #[arg(long, default_value = "runs")]
    runs_dir: PathBuf,

    /// `python` interpreter that has `swebench` installed.
    #[arg(long, default_value = "python")]
    python_bin: String,

    /// `docker` binary.
    #[arg(long, default_value = "docker")]
    docker_bin: String,

    /// Results JSON output path (default: <results_dir>/results-<arm>-<run_id>.json).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Print the plan and exit without Docker/keys/spend.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    let mut instances = load_instances(&args.instances_json)?;
    if !args.instance_ids.is_empty() {
        let wanted: std::collections::HashSet<&str> =
            args.instance_ids.iter().map(String::as_str).collect();
        instances.retain(|i| wanted.contains(i.instance_id.as_str()));
    }
    if instances.is_empty() {
        bail!(
            "no instances selected from {} (check --instance-ids)",
            args.instances_json.display()
        );
    }
    let run_id = args.run_id.clone().unwrap_or_else(default_run_id);
    let model_name = arm_model_name(args.arm.as_str());

    if args.dry_run {
        print_plan(args.arm, &args, &instances, &run_id);
        return Ok(());
    }

    std::fs::create_dir_all(&args.results_dir)
        .with_context(|| format!("create results dir {}", args.results_dir.display()))?;
    std::fs::create_dir_all(&args.runs_dir)
        .with_context(|| format!("create runs dir {}", args.runs_dir.display()))?;
    let results_dir = args
        .results_dir
        .canonicalize()
        .unwrap_or_else(|_| args.results_dir.clone());
    let runs_dir = args
        .runs_dir
        .canonicalize()
        .unwrap_or_else(|_| args.runs_dir.clone());
    let instance_ids: Vec<String> = instances.iter().map(|i| i.instance_id.clone()).collect();

    // Agent arm: run aura per instance → patches. Other arms have no run metrics.
    let runs_by_id: HashMap<String, agent::InstanceRun> = if args.arm == Arm::Agent {
        run_agent(&args, &instances, &run_id).await?
    } else {
        HashMap::new()
    };

    // Build predictions (or use gold) and grade.
    let predictions = match args.arm {
        Arm::Oracle => Predictions::Gold,
        Arm::Agent | Arm::Noop => {
            let lines: Vec<_> = instances
                .iter()
                .map(|inst| {
                    let patch = runs_by_id
                        .get(&inst.instance_id)
                        .map(|r| r.patch.as_str())
                        .unwrap_or("");
                    prediction_line(&inst.instance_id, model_name, patch)
                })
                .collect();
            let path = runs_dir.join(format!("predictions-{}-{run_id}.jsonl", args.arm.as_str()));
            std::fs::write(&path, predictions_jsonl(&lines))
                .with_context(|| format!("write predictions {}", path.display()))?;
            tracing::info!(path = %path.display(), "predictions written");
            Predictions::File(path)
        }
    };

    let grade = grader::grade(
        &GraderConfig {
            python_bin: args.python_bin.clone(),
            dataset_name: args.dataset_name.clone(),
            split: args.split.clone(),
            run_id: run_id.clone(),
            max_workers: args.max_workers,
            model_name: model_name.to_string(),
            runs_dir: runs_dir.clone(),
            instance_ids: instance_ids.clone(),
            namespace: args.namespace.clone(),
        },
        &predictions,
    )
    .await?;

    // Join the harness verdict with each instance's agent-run metrics.
    let resolved = grade.resolved_set();
    let empty = grade.empty_set();
    let errored = grade.error_set();
    let results: Vec<InstanceResult> = instances
        .iter()
        .map(|inst| {
            let id = inst.instance_id.as_str();
            let run = runs_by_id.get(id);
            InstanceResult {
                instance_id: inst.instance_id.clone(),
                repo: inst.repo.clone(),
                resolved: resolved.contains(id),
                empty_patch: empty.contains(id)
                    || run
                        .map(|r| r.patch.trim().is_empty())
                        .unwrap_or(args.arm == Arm::Noop),
                errored: errored.contains(id) || run.map(|r| r.error.is_some()).unwrap_or(false),
                patch_bytes: run.map(|r| r.patch.len()).unwrap_or(0),
                latency_ms: run.map(|r| r.latency_ms).unwrap_or(0),
                input_tokens: run.map(|r| r.input_tokens).unwrap_or(0),
                output_tokens: run.map(|r| r.output_tokens).unwrap_or(0),
                cached_input_tokens: run.map(|r| r.cached_input_tokens).unwrap_or(0),
                cost_micro_usd: run.map(|r| r.cost_micro_usd).unwrap_or(0),
                error: run.and_then(|r| r.error.clone()),
            }
        })
        .collect();

    let report = aggregate(
        ReportMeta {
            run_id: run_id.clone(),
            dataset: args.dataset_name.clone(),
            split: args.split.clone(),
            arm: args.arm.as_str().to_string(),
            model: model_name.to_string(),
        },
        results,
    );
    print_table(&report);

    let out = args.out.clone().unwrap_or_else(|| {
        results_dir.join(format!("results-{}-{run_id}.json", args.arm.as_str()))
    });
    std::fs::write(&out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("write results {}", out.display()))?;
    tracing::info!(path = %out.display(), "results written");
    Ok(())
}

/// Run the agent arm: preflight images, then a bounded-concurrency sweep of
/// `aura prompt` inside each eval image. Returns the runs keyed by instance id.
async fn run_agent(
    args: &Args,
    instances: &[SweInstance],
    run_id: &str,
) -> Result<HashMap<String, agent::InstanceRun>> {
    let aura_bin = args.aura_bin.as_ref().context(
        "the agent arm requires --aura-bin (the `--features bench-bash` static-musl build)",
    )?;
    if !aura_bin.exists() {
        bail!("aura binary not found at {}", aura_bin.display());
    }
    let rg_bin = args.rg_bin.as_ref().context(
        "the agent arm requires --rg-bin (a static-musl ripgrep bundled into each eval image)",
    )?;
    if !rg_bin.exists() {
        bail!("rg binary not found at {}", rg_bin.display());
    }
    // Split `<provider>/<model>`; read the key from the canonical `AURA_API_KEY`
    // (the bench fixes the env-var name — it's not a user knob).
    let (provider, model) = parse_model(&args.model);
    let api_key_env = AURA_API_KEY_ENV.to_string();
    let key_value = std::env::var(&api_key_env).map_err(|_| {
        anyhow::anyhow!("the agent arm needs the model key in ${api_key_env} (set it in .env)")
    })?;

    // Preflight: ensure every eval image is present, pulling prebuilt Hub images
    // (`--namespace swebench`) on demand. A local-build name (`--namespace none`)
    // that isn't present can't be pulled — point the user at prepare_images.
    let mut missing = Vec::new();
    for inst in instances {
        if agent::image_exists(&args.docker_bin, &inst.image_key).await {
            continue;
        }
        tracing::info!(image = %inst.image_key, "eval image not present; pulling");
        if !agent::pull_image(&args.docker_bin, &inst.image_key).await {
            missing.push(inst.image_key.clone());
        }
    }
    if !missing.is_empty() {
        bail!(
            "{} eval image(s) missing and not pullable, e.g. `{}`. For a local-build \
             run pass `--namespace none` and pre-build:\n  \
             python -m swebench.harness.prepare_images --dataset_name {} --split {} \
             --instance_ids {}",
            missing.len(),
            missing[0],
            args.dataset_name,
            args.split,
            instances
                .iter()
                .map(|i| i.instance_id.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let config_json = agent::render_agent_config(&AgentModel {
        provider: provider.clone(),
        model: model.clone(),
        api_key_env: api_key_env.clone(),
        base_url: args.base_url.clone(),
    });

    let docker_bin = &args.docker_bin;
    let cfg = &config_json;
    let key_env = &api_key_env;
    let key_val = &key_value;
    let timeout = args.prompt_timeout;
    let run_id_ref = run_id;
    let trace_arm_dir: Option<PathBuf> =
        (!args.no_trace).then(|| args.trace_dir.join(run_id).join(args.arm.as_str()));
    let trace_ref = trace_arm_dir.as_deref();
    tracing::info!(
        instances = instances.len(),
        concurrency = args.concurrency,
        "agent arm: running aura in eval containers"
    );
    let runs: Vec<agent::InstanceRun> = stream::iter(instances.iter())
        .map(|inst| async move {
            let opts = RunOpts {
                docker_bin,
                aura_bin_host: aura_bin,
                rg_bin_host: rg_bin,
                config_json: cfg,
                instance: inst,
                api_key_env: key_env,
                api_key_value: key_val,
                session_id: format!("swe-{run_id_ref}-{}", inst.instance_id),
                container_name: format!("aura-swe-{run_id_ref}-{}", inst.instance_id),
                prompt_timeout_secs: timeout,
                trace_dir: trace_ref,
            };
            let run = agent::run_instance(opts).await;
            if let Some(err) = &run.error {
                tracing::warn!(instance = %run.instance_id, error = %err, "instance errored");
            } else {
                tracing::info!(instance = %run.instance_id, patch_bytes = run.patch.len(), "instance done");
            }
            run
        })
        .buffer_unordered(args.concurrency.max(1))
        .collect()
        .await;

    Ok(runs
        .into_iter()
        .map(|r| (r.instance_id.clone(), r))
        .collect())
}

fn print_plan(arm: Arm, args: &Args, instances: &[SweInstance], run_id: &str) {
    println!("dry run — dataset: {} ({})", args.dataset_name, args.split);
    println!("arm: {}   run_id: {run_id}", arm.as_str());
    println!("instances: {}", instances.len());
    for inst in instances {
        println!("  {}  [{}]", inst.instance_id, inst.image_key);
    }
    match arm {
        Arm::Agent => println!(
            "plan: {} aura-in-container runs (concurrency {}) → predictions → grade ({} workers)",
            instances.len(),
            args.concurrency.max(1),
            args.max_workers
        ),
        Arm::Oracle => println!("plan: grade gold patches (no aura, no keys)"),
        Arm::Noop => println!("plan: grade empty patches (no aura, no keys)"),
    }
    println!("(no Docker started, no API calls made)");
}
