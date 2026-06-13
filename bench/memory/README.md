# aura-bench-memory — memory benchmark

Measures how much each memory backend (`mem0` / `openviking`) helps **the real
Aura agent** answer questions about a long, multi-session conversation it can no
longer see in context — the analog of how upstream's OpenViking benchmark drives
OpenClaw. The answering agent runs the full Aura loop (recall + memory tools);
judging runs on DeepSeek (`--judge-model`, default `deepseek-v4-pro`).

The bench drives the agent as a **black box**: it execs the `aura` binary
(`aura gateway` once per arm, then a concurrent `aura prompt` per question over
it) — no linkage to Aura's agent stack. Its one library dependency is
`aura-memory`, used by the `ingest` bin to populate the backend directly.

> ⚠️ **Not a CI / `cargo test` target.** The bins start a real `aura` gateway and
> hit live mem0/openviking + DeepSeek — they spend money. Run them by hand.

## You must build `aura` with `--features bench-readonly-memory`

QA drives the *real* agent, whose loop writes memory after every turn
(`on_job_complete` / `on_session_end`). Left alone, each QA turn would write its
question+answer into the conversation's recall scope and pollute later questions
(the same exposure upstream's OpenClaw bench has). The `bench-readonly-memory`
feature wraps the backend so it **recalls + exposes tools but never writes** —
so QA stays contamination-free *by construction*, even as the real agent:

```bash
cargo build --release --features bench-readonly-memory   # produces ./target/release/aura
```

Point the bench at that binary with `--aura-bin`. (`ingest` is the sole writer,
and it bypasses the agent entirely.)

## How it works

Two phases, coupled by a **manifest**:

1. **`ingest`** — replays a conversation into a backend via its concrete methods
   (`add_message`/`add_turn`, then commit + poll extraction to true completion),
   under a per-conversation scope `user_id`. Direct, not through the agent:
   LOCOMO is a two-human dialogue with no assistant and gold answers hinge on
   verbatim text, so generating one side would corrupt it.
2. **`run`** — for one arm: build a gateway config, start `aura gateway` against
   it, then answer each question with a concurrent `aura prompt --json --session
   <fresh> -- <prompt>`. **By default the config is self-contained** — a fresh
   workspace, a freshly-minted encryption key, and a DeepSeek answer model
   (`--answer-model`, keyed from `DEEPSEEK_API_KEY`); the vault auto-creates and
   `aura gateway start` mints its own token, so nothing pre-exists. Pass
   `--aura-config` to derive from your own `aura.json` instead (reuse its
   workspace/keys/llm, overwrite only the memory section). Each `aura prompt`
   runs with `USER=<conv-scope>` → message sender → session user → recall scope,
   matching what `ingest` populated. Answers are judged by DeepSeek; per-category
   accuracy + token-overlap F1 are reported, alongside answer-time (latency),
   answer-side token counts, and cost (read back per question from Aura's cost
   ledger — see the token/cost caveat).

- **arm → memory**: `openviking`/`mem0` enable that backend in the gateway
  config; `noop`/`oracle` turn memory off (`oracle` prepends the whole
  conversation to the prompt — the ceiling; `noop` is the floor).
- **isolation**: each conversation has a unique `user_id`
  (`{testset}-{run_id}-{arm}-conv{N}`); QA never writes (the feature), so reruns
  and conversations never collide.

## Credentials & config

| Var / flag | Used by | Notes |
| --- | --- | --- |
| `--aura-config` | `run` | **optional override.** Omit → self-contained config (fresh workspace + minted key + the configurable answer model below). Given → derive from yours (reuse its workspace/keys/llm), overwriting only the `memory` section. |
| `--answer-model` / `--answer-provider` / `--answer-api-key-env` / `--answer-base-url` (env: `AURA_ANSWER_MODEL` / `AURA_ANSWER_PROVIDER` / `AURA_ANSWER_API_KEY_ENV` / `AURA_ANSWER_BASE_URL`) | `run` (self-contained only) | the model Aura answers with. Defaults reproduce the DeepSeek answerer (`deepseek` / `deepseek-chat` / `DEEPSEEK_API_KEY`, provider default endpoint); point them at any Aura-supported provider to benchmark its cost/quality. Ignored with `--aura-config`. |
| `--aura-bin` | `run` | the `aura` built with `--features bench-readonly-memory` (default `aura` on PATH). |
| `DEEPSEEK_API_KEY` | `run` | judge model always; the self-contained answer model when `AURA_ANSWER_API_KEY_ENV` is left at its default — the gateway inherits whichever key env you point it at |
| `OPENVIKING_API_KEY` | `ingest` + `run` openviking arm | matches `ov.conf`'s root key; the agent resolves it from this env var |
| `OPENVIKING_ENDPOINT` | `ingest` openviking arm | or `--openviking-endpoint` |
| `MEM0_BASE_URL` | `ingest`/`run` mem0 arm | self-hosted docker server (default `http://127.0.0.1:8765`, shares OV's DeepSeek+Qwen3 stack); blank it to use the cloud Platform |
| `MEM0_API_KEY` | `ingest`/`run` mem0 arm | **cloud only** — a dedicated throwaway project; not needed when `MEM0_BASE_URL` points at the docker server |

So you write **no `aura.json`** and need no pre-existing `~/.aura` — the bench
mints a fresh, isolated workspace per arm (so nothing of yours is touched and
there's no pollution). Those workspaces (config + vault + sessions + logs) go
under `runs/` (gitignored, override with `WS_ROOT` / `--workspace-root`;
defaults to the system temp dir when the bins are run directly). The **answer
model** (the model Aura answers with) defaults to DeepSeek `deepseek-chat`, but
provider, model, key-env, and base URL are all configurable —
`--answer-provider` / `--answer-model` / `--answer-api-key-env` /
`--answer-base-url` (or the `ANSWER_*` env vars in `run.sh`) — so you can
benchmark any provider's cost/quality (the per-question token/cost metrics make
the comparison direct). With `--aura-config` the answer model is instead that
config's `default-llm` and QA sessions land in its workspace (harmless — memory
writes are off under the feature). No other `aura gateway` may run in the
workspace in use.

## Bring up the backends (docker)

Both real arms hit local servers from `docker-compose.yml` (gitignored `.env`
supplies their keys + the shared DeepSeek/Qwen3 endpoints). Start them first:

```bash
cd bench/memory && docker compose up -d   # openviking :1933, mem0 :8765, qdrant :6333
```

- **openviking** (`:1933`) — extraction LLM + embedder from `ov.conf` (`${...}` from `.env`).
- **mem0** (`:8765`) — a thin `mem0` lib server (`mem0-server/`) backed by **qdrant**,
  wired to the **same** DeepSeek (LLM) + Qwen3 (embedder) as OpenViking, so the two
  arms are aligned by construction. This is the default for the mem0 arm (the slow
  managed cloud Platform is opt-in via a blank `MEM0_BASE_URL` + `MEM0_API_KEY`).

An embedding server must be reachable at `OPENVIKING_EMBEDDING_URL` (both arms
share it). Bring it down with `docker compose down` (add `-v` to wipe qdrant).

## Usage

Credentials + endpoints live in `bench/memory/.env` (gitignored). Copy the
template and fill it in:

```bash
cp bench/memory/.env.example bench/memory/.env   # then fill DEEPSEEK_API_KEY etc.
```

Easiest — `run.sh` builds the read-only `aura`, auto-loads `.env`, ingests
as needed, and prints a floor → backend → ceiling table:

```bash
ARMS="noop oracle openviking" CONVERSATIONS=3 QUESTIONS=10 bench/memory/run.sh
```

Or drive the bins directly (they read the process env, so build + source first):

```bash
cargo build --release --features bench-readonly-memory   # the read-only aura
set -a; . bench/memory/.env; set +a                      # export the creds

# Ingest (direct to the backend), then QA through the real agent (config auto-generated):
cargo run -p aura-bench-memory --bin ingest -- --arm openviking --dataset locomo10.json
cargo run -p aura-bench-memory --bin run -- --arm openviking --dataset locomo10.json \
    --aura-bin ./target/release/aura --manifest manifest-openviking-<run_id>.json

# Floor / ceiling — memory off, no ingest:
cargo run -p aura-bench-memory --bin run -- --arm noop   --dataset locomo10.json --aura-bin ./target/release/aura
cargo run -p aura-bench-memory --bin run -- --arm oracle --dataset locomo10.json --aura-bin ./target/release/aura

# Plan / cost without starting anything (no config or keys needed):
cargo run -p aura-bench-memory --bin run -- --arm openviking --dataset locomo10.json --dry-run
```

Each `run` evaluates one arm and writes its full report to
`results/results-<arm>-<run_id>.json` (summary scores + every question's answer,
judge reason, answer-time, token counts, and cost). That folder is **gitignored**
(local only, like `bench-out/`) — the JSONs are large, LLM-generated, and
rewritten each run, so they're regenerated rather than committed. `run.sh`
compares the run's JSONs into the floor → backend → ceiling table, which now
also carries `lat_ms` / `in_tok` / `out_tok` / `cost$` columns per arm.

## Inspecting results — error analysis

Two read-only helper scripts trace a scored question back to its sources, so you
can tell *why* an answer was wrong — gold noise vs recall miss vs extraction loss
vs integration failure. Both read `results/results-<arm>-<run_id>.json` plus the
run's artifacts; neither starts a gateway or spends anything. (Run-id suffix:
`noop`/`oracle` were `full10`, `mem0`/`openviking` were `full10b` — pass whichever
`<run_id>` produced the results you're reading.)

Alongside, every question writes its **verbatim transcript + call-tree trace** to
`trace/<run_id>/<arm>/<session>.{messages,trace}.json` (`aura session history` +
`session export`) — the raw record of what aura recalled, answered, and called per
question. Default-on; `NO_TRACE=1` disables it; `trace/` is gitignored.

**`trace_incorrect.py` — back to the source dialogue.** Matches each incorrect
answer (by `conv_idx` + question) to its LOCOMO `evidence` dia_ids and the
original dialogue turns. LOCOMO's `evidence` often points at a lead-in line while
the answer sits a turn over, so `--context K` prints K turns on each side.

```bash
# the ceiling's misses with surrounding context (best signal for gold noise)
python3 bench/memory/trace_incorrect.py oracle full10 --context 1 --limit 15
# a single conversation
python3 bench/memory/trace_incorrect.py openviking full10b --conv 0
```

**`trace_recall.py` — what aura actually recalled.** Reads each question's exported
transcript (`trace/<run_id>/<arm>/qa-*.messages.json`); the backend's recall rides
there as a `source="recalled_memory"` message. Prints, per question:
question + gold + correct, every recalled block, and aura's final answer — pinning
a wrong answer to a **recall miss** (fact never recalled), an **extraction loss**
(recalled but the detail was generalized away), or an **integration failure**
(recalled but unused).

```bash
# wrong answers in conv 0 and the memory aura saw for each
python3 bench/memory/trace_recall.py openviking full10b --conv 0 --incorrect
# one question, full recall text (--chars 0 = untruncated)
python3 bench/memory/trace_recall.py openviking full10b --conv 0 --q 1 --chars 0
# compare what a different backend recalled for the same question
python3 bench/memory/trace_recall.py mem0 full10b --conv 0 --q 1
```

## Caveats

- **Read-only build is mandatory.** Without `--features bench-readonly-memory`,
  the agent writes QA turns into the recall scope and contaminates the run. The
  bench can't detect that you forgot — make sure `--aura-bin` is the feature
  build.
- **Extraction-model confound:** mem0/openviking extract server-side with their
  *own* LLM, not your answer model. The arms measure the systems as shipped.
- **Adversarial questions are dropped.** LOCOMO category 5 ("adversarial") are
  unanswerable distractors: the question asks about one speaker but the fact
  belongs to the other, so the dataset's `adversarial_answer` is a trap, not a
  gradeable gold (upstream leaves these gold answers empty and excludes the
  category from scoring). The runner drops them before QA, matching upstream — so
  every scored question is genuinely answerable, and the no-memory floor sits
  near 0% instead of being flattered by guesses the judge can't grade.
- Settling: `ingest` polls true completion (openviking commit tasks; mem0's
  events feed) and marks the manifest unsettled otherwise; `run` refuses a
  mismatched/unsettled manifest unless `--allow-unsettled`.
- **Answer-time, tokens, and cost are surfaced** per question, sourced from
  Aura's `cost_records` ledger via a per-question `aura cost show --session <id>
  --json` issued after each answer. The **judge is excluded by construction** —
  it's a standalone DeepSeek `ChatClient` (`llm.rs`) that never writes
  `cost_records`. The number is **whole-turn answer-side spend** (the answer
  call plus any recall/tool sub-calls billed to the session), not just the final
  completion. `input_tokens` is gross input (cached tokens are reported
  separately and not subtracted); for the DeepSeek answerer cache hits are ~0,
  so it reflects the real prompt size. The cost row is written asynchronously
  and races the answer, so the read polls briefly for the row to land; under
  heavy concurrency a turn can still record zeros if the row hasn't committed in
  time. In `--aura-config` mode these rows land in your real workspace ledger
  under the bench's `qa-*` sessions and are left in place (Aura never
  auto-deletes cost history), so they also surface in untargeted `aura cost`
  totals.
- **Oracle assumes the conversation fits** the answer model's context window
  (~26k tokens for LOCOMO); if it doesn't it's truncated and oracle is no longer
  a clean ceiling. Oracle's `input_tokens`/`cost` are also inflated by the whole
  conversation prepended to every prompt — expected, a useful signal, not a
  backend cost.
