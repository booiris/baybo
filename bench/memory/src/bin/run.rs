//! Phase 2 of the memory benchmark: answer each question by driving the REAL
//! Aura agent — one `aura gateway` per arm, then a concurrent `aura prompt`
//! per question routed over it (recall + memory tools run inside the real
//! loop). Each answer is judged by a standalone DeepSeek client, then reported.
//!
//! Per-conversation isolation: each `aura prompt` runs with `USER=<conv-scope>`
//! (→ message sender → session user → recall scope), matching what `ingest`
//! populated. One arm per invocation; run once per arm and compare the JSONs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use aura_bench_memory::agent::{AnswerModel, GatewayHandle, generate_config, prepare_arm_config};
use aura_bench_memory::judge::judge;
use aura_bench_memory::llm::ChatClient;
use aura_bench_memory::report::{QuestionResult, ReportMeta, aggregate, print_table};
use aura_bench_memory::testset::{BenchSample, TestSetKind};
use aura_bench_memory::{Manifest, scope_user_id, token_f1};
use clap::{Parser, ValueEnum};
use futures::{StreamExt, stream};

/// Folded into every question prompt so the agent answers in a judgeable shape
/// regardless of the configured soul. Mirrors upstream's "answer directly".
const ANSWER_INSTRUCTION: &str = "Answer the question directly and concisely with just the specific fact requested. If the information is not available to you, say you do not have it rather than guessing.";

/// Report label for the answer model under `--aura-config` (we don't introspect
/// that config's `default-llm`); self-contained runs report `provider/model`.
const ANSWER_LABEL: &str = "aura-prompt (gateway)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Arm {
    /// Memory off (floor).
    Noop,
    Mem0,
    Openviking,
    /// Memory off, whole conversation in the prompt (ceiling).
    Oracle,
}

impl Arm {
    fn as_str(self) -> &'static str {
        match self {
            Arm::Noop => "noop",
            Arm::Mem0 => "mem0",
            Arm::Openviking => "openviking",
            Arm::Oracle => "oracle",
        }
    }

    /// The memory provider to enable in the gateway's config — `None` turns
    /// memory off (noop/oracle).
    fn memory_provider(self) -> Option<&'static str> {
        match self {
            Arm::Mem0 => Some("mem0"),
            Arm::Openviking => Some("openviking"),
            Arm::Noop | Arm::Oracle => None,
        }
    }

    /// Recall arms need the ingest manifest (for the per-conversation scope).
    fn needs_manifest(self) -> bool {
        self.memory_provider().is_some()
    }
}

#[derive(Parser, Debug)]
#[command(about = "memory benchmark — drive the real Aura agent for one arm and score it")]
struct Args {
    /// Which arm to evaluate.
    #[arg(long, value_enum)]
    arm: Arm,

    /// Which test set to evaluate (must match the manifest for recall arms).
    #[arg(long, value_enum, default_value = "locomo")]
    testset: TestSetKind,

    /// Path to the dataset file for the chosen test set (same file as ingest).
    #[arg(long)]
    dataset: PathBuf,

    /// Manifest from `ingest` (required for mem0 / openviking — carries the
    /// recall scope each conversation was populated under).
    #[arg(long)]
    manifest: Option<PathBuf>,

    /// Optional: derive the gateway config from your aura.json (overwriting only
    /// its memory section), reusing its workspace/keys/llm. Omit to generate a
    /// self-contained config (fresh workspace + minted key + DeepSeek answerer).
    #[arg(long)]
    aura_config: Option<PathBuf>,

    /// Directory to hold generated gateway configs + self-contained workspaces
    /// (one `aura-bench-ws-<run_id>-<arm>` each). Defaults to the system temp
    /// dir; `run-bench.sh` points it under the (gitignored) bench dir.
    #[arg(long)]
    workspace_root: Option<PathBuf>,

    /// The `aura` binary to drive (gateway + prompt).
    #[arg(long, default_value = "aura")]
    aura_bin: String,

    /// OpenViking endpoint the agent's memory backend connects to (host side of
    /// the Docker port-map). Written into the generated config's memory.extra.
    #[arg(long, default_value = "http://127.0.0.1:1933")]
    openviking_endpoint: String,

    /// Self-hosted mem0 OSS server URL. When set, the mem0 arm targets it
    /// (`self_hosted` mode, no key); omit to use the managed cloud Platform.
    #[arg(long)]
    mem0_base_url: Option<String>,

    /// top_k for recall, written into the generated config's memory.extra.
    #[arg(long, default_value_t = 10)]
    top_k: usize,

    /// Answer model for the self-contained config — the `llm` entry the real
    /// agent answers with. Ignored when `--aura-config` is given (yours wins).
    #[arg(long, default_value = "deepseek-chat")]
    answer_model: String,

    /// Provider for the self-contained answer model — any Aura-supported
    /// provider (`deepseek`, `openai`, `anthropic`, …). Self-contained only.
    #[arg(long, default_value = "deepseek")]
    answer_provider: String,

    /// Env var holding the answer provider's API key, written as `api_key_env`
    /// into the generated config. Self-contained only.
    #[arg(long, default_value = "DEEPSEEK_API_KEY")]
    answer_api_key_env: String,

    /// Override the answer provider's base URL (self-contained only). Omit to
    /// use the provider's built-in default endpoint.
    #[arg(long)]
    answer_base_url: Option<String>,

    /// Gateway port for the self-contained config. `0` (default) auto-picks a
    /// free ephemeral port, so repeated/concurrent runs (and a user's own
    /// gateway) never collide on a fixed port.
    #[arg(long, default_value_t = 0)]
    gateway_port: u16,

    /// How many conversations to evaluate, from the start of the dataset.
    #[arg(long, default_value_t = 1)]
    conversations: usize,

    /// Cap on questions per conversation (default: all).
    #[arg(long)]
    questions: Option<usize>,

    /// Judge model id, passed verbatim to the chat-completions API.
    #[arg(long, default_value = "deepseek-v4-pro")]
    judge_model: String,

    /// OpenAI-compatible base URL for the judge (default: DeepSeek).
    #[arg(long)]
    deepseek_base_url: Option<String>,

    /// Per-question ceiling for `aura prompt` (seconds). 10 min: ample for a real
    /// agent turn (recall + tool calls + answer), short enough that a wedged turn
    /// fails fast instead of hanging the whole run for an hour.
    #[arg(long, default_value_t = 600)]
    prompt_timeout: u64,

    /// Results JSON output path (default: results-{arm}-{run_id}.json).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Print the planned call counts and exit without spending anything.
    #[arg(long)]
    dry_run: bool,

    /// Max questions answered concurrently (concurrent `aura prompt`s over the
    /// one gateway).
    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// Run QA even if the manifest reports extraction did not settle cleanly.
    #[arg(long)]
    allow_unsettled: bool,
}

/// One independent QA unit. `convo` is `Some` only for the oracle arm (the
/// rendered conversation, shared per-conversation via `Arc`). `seq` restores
/// stable output ordering after the unordered run.
struct QaTask {
    seq: usize,
    conv_idx: usize,
    category: String,
    question: String,
    gold: String,
    user_id: String,
    convo: Option<Arc<String>>,
    session_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    let testset_name = args.testset.name();
    let mut samples = args.testset.test_set().load(&args.dataset)?;
    // LOCOMO category 5 ("adversarial") questions are unanswerable distractors:
    // the question asks about one speaker but the fact belongs to the other, so
    // the dataset's `adversarial_answer` is a trap, not a gradeable gold. Upstream
    // leaves these gold answers empty and drops the category from its denominator;
    // we do the same so every scored question is genuinely answerable.
    for sample in &mut samples {
        sample.questions.retain(|q| !q.adversarial);
    }
    let n_conv = args.conversations.min(samples.len());
    let n_q = args.questions.unwrap_or(usize::MAX);

    if args.dry_run {
        print_plan(
            args.arm,
            testset_name,
            &samples[..n_conv],
            n_q,
            args.concurrency,
        );
        return Ok(());
    }

    let manifest = match args.manifest.as_ref() {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read manifest {}", path.display()))?;
            Some(serde_json::from_str::<Manifest>(&raw).context("parse manifest")?)
        }
        None => None,
    };
    if args.arm.needs_manifest() {
        let manifest = manifest.as_ref().with_context(|| {
            format!(
                "the {} arm requires --manifest from ingest",
                args.arm.as_str()
            )
        })?;
        validate_manifest(
            manifest,
            args.arm,
            testset_name,
            &samples,
            n_conv,
            args.allow_unsettled,
        )?;
    }
    let run_id = manifest
        .as_ref()
        .map(|m| m.run_id.clone())
        .unwrap_or_else(default_run_id);

    let judge_llm = ChatClient::new(&args.judge_model, args.deepseek_base_url.as_deref())?;

    // Config for this arm's gateway. Default: generate a self-contained one
    // (fresh workspace + minted key + DeepSeek answerer). With `--aura-config`:
    // derive from yours, overwriting only the memory section.
    let memory = arm_memory(
        args.arm,
        &args.openviking_endpoint,
        args.mem0_base_url.as_deref(),
        args.top_k,
    );
    let ws_root = args
        .workspace_root
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&ws_root)
        .with_context(|| format!("create workspace root {}", ws_root.display()))?;
    let (config_path, admin_addr) = match args.aura_config.as_deref() {
        Some(base) => {
            let out = ws_root.join(format!("aura-bench-{run_id}-{}.json", args.arm.as_str()));
            prepare_arm_config(base, memory, out)?
        }
        None => {
            let ws = ws_root.join(format!("aura-bench-ws-{run_id}-{}", args.arm.as_str()));
            // 0 → grab a free ephemeral port so repeated/concurrent runs (and the
            // user's own gateway) never collide on a fixed port.
            let gateway_port = match args.gateway_port {
                0 => pick_free_port()?,
                p => p,
            };
            let answer = AnswerModel {
                provider: args.answer_provider.clone(),
                model: args.answer_model.clone(),
                api_key_env: args.answer_api_key_env.clone(),
                base_url: args.answer_base_url.clone(),
            };
            generate_config(&ws, memory, &answer, gateway_port)?
        }
    };
    tracing::info!(arm = args.arm.as_str(), %admin_addr, "starting aura gateway");
    let gateway = GatewayHandle::start(&args.aura_bin, &config_path, admin_addr).await?;
    tracing::info!(arm = args.arm.as_str(), %run_id, conversations = n_conv, concurrency = args.concurrency, "QA start");

    // Flatten to independent per-question tasks; the oracle convo is shared
    // per conversation via Arc.
    let mut tasks = Vec::new();
    let mut seq = 0usize;
    for (conv_idx, sample) in samples.iter().take(n_conv).enumerate() {
        let user_id = resolve_user_id(args.arm, testset_name, &manifest, &run_id, conv_idx)?;
        let convo = (args.arm == Arm::Oracle).then(|| Arc::new(sample.conversation.render()));
        for (q_idx, q) in sample.questions.iter().take(n_q).enumerate() {
            tasks.push(QaTask {
                seq,
                conv_idx,
                category: q.category.clone(),
                question: q.question.clone(),
                gold: q.gold_answer.clone(),
                user_id: user_id.clone(),
                convo: convo.clone(),
                session_id: format!("qa-{run_id}-{}-c{conv_idx}-q{q_idx}", args.arm.as_str()),
            });
            seq += 1;
        }
    }

    let gateway_ref = &gateway;
    let judge_ref = &judge_llm;
    let timeout = args.prompt_timeout;
    let collected: Vec<Result<(usize, QuestionResult)>> = stream::iter(tasks)
        .map(|task| async move {
            let prompt = build_prompt(&task.question, task.convo.as_deref().map(String::as_str));
            let started = Instant::now();
            let outcome = gateway_ref
                .run_prompt(&task.user_id, &task.session_id, &prompt, timeout, true)
                .await;
            let latency_ms = started.elapsed().as_millis() as u64;

            let answer = match outcome {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::warn!(conv = task.conv_idx, seq = task.seq, error = %e, "prompt failed; scored incorrect");
                    return Ok((
                        task.seq,
                        QuestionResult {
                            conv_idx: task.conv_idx,
                            category: task.category,
                            question: task.question,
                            gold: task.gold,
                            answer: format!("<prompt error: {e}>"),
                            correct: false,
                            f1: 0.0,
                            judge_reason: "aura prompt failed; not judged".to_string(),
                            latency_ms,
                            session_id: task.session_id.clone(),
                            input_tokens: 0,
                            output_tokens: 0,
                            cost_micro_usd: 0,
                        },
                    ));
                }
            };

            // A cost-read failure degrades to zeros — it must not fail the question.
            let (input_tokens, output_tokens, cost_micro_usd) = match gateway_ref
                .session_cost(&task.session_id)
                .await
            {
                Ok(metrics) => metrics,
                Err(e) => {
                    tracing::warn!(conv = task.conv_idx, seq = task.seq, error = %e, "cost read failed; recording zeros");
                    (0, 0, 0)
                }
            };

            let verdict = judge(judge_ref, &task.question, &task.gold, &answer)
                .await
                .with_context(|| format!("judge conv {} seq {}", task.conv_idx, task.seq))?;
            let f1 = token_f1(&task.gold, &answer);
            tracing::info!(conv = task.conv_idx, seq = task.seq, correct = verdict.correct, "scored");
            Ok((
                task.seq,
                QuestionResult {
                    conv_idx: task.conv_idx,
                    category: task.category,
                    question: task.question,
                    gold: task.gold,
                    answer,
                    correct: verdict.correct,
                    f1,
                    judge_reason: verdict.reason,
                    latency_ms,
                    session_id: task.session_id.clone(),
                    input_tokens,
                    output_tokens,
                    cost_micro_usd,
                },
            ))
        })
        .buffer_unordered(args.concurrency.max(1))
        .collect()
        .await;

    gateway.shutdown().await;

    let mut ordered: Vec<(usize, QuestionResult)> = Vec::with_capacity(collected.len());
    for item in collected {
        ordered.push(item?);
    }
    ordered.sort_by_key(|(seq, _)| *seq);
    let results: Vec<QuestionResult> = ordered.into_iter().map(|(_, r)| r).collect();

    let answer_model = if args.aura_config.is_some() {
        ANSWER_LABEL.to_string()
    } else {
        format!(
            "{}/{} (aura gateway)",
            args.answer_provider, args.answer_model
        )
    };
    let report = aggregate(
        ReportMeta {
            run_id: run_id.clone(),
            testset: testset_name.to_string(),
            arm: args.arm.as_str().to_string(),
            answer_model,
            judge_model: args.judge_model.clone(),
            conversations: n_conv,
        },
        results,
    );
    print_table(&report);

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("results-{}-{run_id}.json", args.arm.as_str())));
    std::fs::write(&out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("write results {}", out.display()))?;
    tracing::info!(path = %out.display(), "results written");

    Ok(())
}

/// The prompt sent to `aura prompt`: the answer instruction + question, with
/// the whole conversation prepended for the oracle arm.
fn build_prompt(question: &str, convo: Option<&str>) -> String {
    match convo {
        Some(convo) => format!(
            "<conversation>\n{convo}\n</conversation>\n\n{ANSWER_INSTRUCTION}\nQuestion: {question}"
        ),
        None => format!("{ANSWER_INSTRUCTION}\nQuestion: {question}"),
    }
}

/// Recall scope for the `USER` env: the manifest's per-conversation id for
/// recall arms (must match ingest), else a fresh key (memory is off, so it's
/// only a label).
fn resolve_user_id(
    arm: Arm,
    testset: &str,
    manifest: &Option<Manifest>,
    run_id: &str,
    conv_idx: usize,
) -> Result<String> {
    if arm.needs_manifest() {
        let manifest = manifest
            .as_ref()
            .context("recall arm requires a manifest")?;
        manifest
            .user_id_for(conv_idx)
            .map(str::to_string)
            .with_context(|| format!("manifest has no user_id for conversation {conv_idx}"))
    } else {
        Ok(scope_user_id(testset, run_id, arm.as_str(), conv_idx))
    }
}

/// Fail fast on a manifest that can't have been produced for this run.
fn validate_manifest(
    manifest: &Manifest,
    arm: Arm,
    testset: &str,
    samples: &[BenchSample],
    n_conv: usize,
    allow_unsettled: bool,
) -> Result<()> {
    if manifest.testset != testset {
        anyhow::bail!(
            "manifest testset '{}' != --testset '{}' — wrong manifest",
            manifest.testset,
            testset
        );
    }
    if manifest.arm != arm.as_str() {
        anyhow::bail!(
            "manifest arm '{}' != --arm '{}' — wrong manifest",
            manifest.arm,
            arm.as_str()
        );
    }
    if !manifest.settled && !allow_unsettled {
        anyhow::bail!(
            "manifest reports extraction did not settle cleanly; re-ingest or pass --allow-unsettled to override"
        );
    }
    for conv_idx in 0..n_conv {
        let scope = manifest
            .conversations
            .iter()
            .find(|c| c.conv_idx == conv_idx)
            .with_context(|| format!("manifest is missing conversation {conv_idx}"))?;
        let dataset_id = samples.get(conv_idx).and_then(|s| s.sample_id.as_deref());
        if let (Some(manifest_id), Some(dataset_id)) = (scope.sample_id.as_deref(), dataset_id)
            && manifest_id != dataset_id
        {
            anyhow::bail!(
                "manifest conversation {conv_idx} sample_id '{manifest_id}' != dataset '{dataset_id}' — manifest is for a different dataset"
            );
        }
    }
    Ok(())
}

fn print_plan(arm: Arm, testset: &str, samples: &[BenchSample], n_q: usize, concurrency: usize) {
    let questions: usize = samples.iter().map(|s| s.questions.len().min(n_q)).sum();
    println!("dry run — testset: {testset}  arm: {}", arm.as_str());
    println!("conversations: {}", samples.len());
    println!("questions: {questions}");
    println!(
        "calls: {questions} aura-prompt (real agent) + {questions} judge + {questions} cost-show"
    );
    println!("concurrency: {}", concurrency.max(1));
    println!("(no gateway started, no API calls made)");
}

/// The `memory` config section for an arm: openviking/mem0 enable that backend
/// (extra carries the endpoint + the env-var name holding the api key, resolved
/// by the runtime); noop/oracle turn memory off.
fn arm_memory(
    arm: Arm,
    openviking_endpoint: &str,
    mem0_base_url: Option<&str>,
    top_k: usize,
) -> serde_json::Value {
    match arm {
        Arm::Openviking => serde_json::json!({
            "enabled": true,
            "provider": "openviking",
            "extra": {
                "endpoint": openviking_endpoint,
                "api_key_name": "OPENVIKING_API_KEY",
                "account": "default",
                "top_k": top_k,
            },
        }),
        Arm::Mem0 => {
            let mut extra = serde_json::json!({
                "api_key_name": "MEM0_API_KEY",
                "rerank": true,
                "top_k": top_k,
            });
            // A base URL means the self-hosted OSS server (no key needed).
            if let Some(url) = mem0_base_url {
                extra["self_hosted"] = serde_json::json!(true);
                extra["base_url"] = serde_json::json!(url);
            }
            serde_json::json!({ "enabled": true, "provider": "mem0", "extra": extra })
        }
        Arm::Noop | Arm::Oracle => serde_json::json!({ "enabled": false }),
    }
}

fn default_run_id() -> String {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{ts}-{}", &suffix[..6])
}

/// Grab a currently-free ephemeral port by binding `127.0.0.1:0` and reading the
/// port the OS assigned. The listener drops immediately so the `aura gateway`
/// child can bind it — a tiny TOCTOU window that's fine for a local bench.
fn pick_free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("bind ephemeral port for gateway")?;
    let port = listener
        .local_addr()
        .context("read ephemeral gateway port")?
        .port();
    Ok(port)
}
