"""Harbor (Terminal-Bench 2.0) installed-agent adapter for Aura.

Runs the real `aura` binary INSIDE each Harbor task container — the
leaderboard-comparable way, graded by the task's own verifier. The container
*is* the sandbox, so aura runs with `sandbox.mode = none` (its Bash executes
commands directly — no bwrap, no work-dir jail). Mirrors Harbor's bundled
installed agents (see harbor/agents/installed/codex.py); the aura.json schema +
install steps are ported verbatim from the tb/1.0 adapter
(bench/terminal-bench-1.0/tb_adapter/aura_agent.py).

Run via Harbor's --agent-import-path (no fork). From bench/terminal-bench-2.0/:

    uv run harbor run \\
        -d terminal-bench/terminal-bench-2 \\
        --agent-import-path harbor_adapter.aura_agent:AuraAgent \\
        -m deepseek/deepseek-v4-flash
"""

import asyncio
import json
import shlex
import tempfile
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths

# Where the binary + config + key live in the container (the base class already
# mkdir's /installed-agent in setup()).
_CONTAINER_DIR = "/installed-agent"
_CONFIG_PATH = f"{_CONTAINER_DIR}/aura.json"
_BIN_PATH = f"{_CONTAINER_DIR}/aura"
_AURA_HOME = f"{_CONTAINER_DIR}/aura-home"
_KEY_PATH = f"{_CONTAINER_DIR}/enc.key"
# Fixed session id for the single prompt per task, so the trace can be exported.
_SESSION_ID = "aura-tb"
# Cap the post-run trace export so a wedged container during cleanup can't hang the
# trial past the agent timeout (the in-shell `|| true` doesn't guard a hung exec).
_EXPORT_TIMEOUT_SECS = 30.0

# Static-musl aura binary copied into each container. Repo-root-relative (this
# file is bench/terminal-bench-2.0/harbor_adapter/); AURA_BIN overrides.
_DEFAULT_BINARY = (
    Path(__file__).resolve().parents[3]
    / "target"
    / "x86_64-unknown-linux-musl"
    / "release"
    / "aura"
)

# `-m <provider>/<model>` → the env var aura reads the provider key from.
# aura.json references the key by name (`api_key_env`); the value is injected at
# run time via the exec env, never written to disk.
_PROVIDER_KEY_ENV = {
    "deepseek": "DEEPSEEK_API_KEY",
    "openai": "OPENAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "gemini": "GEMINI_API_KEY",
    "google": "GEMINI_API_KEY",
    "openrouter": "OPENROUTER_API_KEY",
}


class AuraAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "aura"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        # Model: Harbor's `-m <provider>/<model>` (self.model_name) or AURA_MODEL.
        model_name = self.model_name or self._get_env("AURA_MODEL")
        if not model_name:
            raise RuntimeError(
                "no model selected: pass `-m <provider>/<model>` or set AURA_MODEL"
            )
        provider, sep, model = model_name.partition("/")
        if not sep:
            provider, model = "deepseek", model_name
        self._provider = provider
        self._model = model

        key_env = _PROVIDER_KEY_ENV.get(provider)
        if key_env is None:
            raise ValueError(
                f"unknown provider '{provider}' in model '{model_name}'; "
                f"known providers: {sorted(_PROVIDER_KEY_ENV)}"
            )
        self._key_env = key_env
        # AURA_API_KEY (provider-agnostic) wins, else the provider's own env var.
        self._key_value = self._get_env("AURA_API_KEY") or self._get_env(key_env)
        if not self._key_value:
            raise RuntimeError(
                f"set AURA_API_KEY or ${key_env} to run the aura agent for "
                f"provider '{provider}'"
            )
        self._base_url = self._get_env("AURA_BASE_URL") or None

        self._aura_bin = Path(self._get_env("AURA_BIN") or _DEFAULT_BINARY)
        if not self._aura_bin.is_file():
            raise RuntimeError(
                f"aura binary not found at {self._aura_bin}. Build it with "
                "`cargo build --release --target x86_64-unknown-linux-musl "
                "--features bench-bash -p aura`, or set AURA_BIN."
            )

    def _aura_config(self) -> dict:
        """aura.json rendered into the container — `none` sandbox, a
        self-contained state dir, and the provider/model under test."""
        entry = {
            "name": "agent",
            "provider": self._provider,
            "model": self._model,
            "api_key_env": self._key_env,
        }
        if self._base_url:
            entry["base_url"] = self._base_url
        return {
            "llm": [entry],
            "default-llm": "agent",
            "channels": {"cli": {"enabled": True}},
            "workspace": {"path": _AURA_HOME},
            "security": {
                "encryption_key_file": _KEY_PATH,
                "leak_detection_enabled": False,
            },
            # Required to pass config validation even though `aura prompt` runs
            # in-process and never binds.
            "gateway": {"bind_address": "127.0.0.1", "port": 8723},
            "sandbox": {"mode": "none"},
            "cost": {"rate_limit": {"max_requests": 1_000_000}},
        }

    async def install(self, environment: BaseEnvironment) -> None:
        # Upload the static-musl binary + a pre-rendered config; put the binary on
        # PATH, mint a per-container vault key, ensure TLS roots, and hand the
        # install dir to the agent user. The container is the sandbox.
        await environment.upload_file(self._aura_bin, _BIN_PATH)
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(self._aura_config(), f)
            cfg = Path(f.name)
        try:
            await environment.upload_file(cfg, _CONFIG_PATH)
        finally:
            cfg.unlink(missing_ok=True)

        await self.exec_as_root(
            environment,
            command=(
                f"install -m 0755 {_BIN_PATH} /usr/local/bin/aura\n"
                f"mkdir -p {_AURA_HOME}\n"
                f"od -An -tx1 -N32 /dev/urandom | tr -d ' \\n' > {_KEY_PATH}\n"
                # aura needs TLS roots for the HTTPS LLM call. Base images usually
                # ship ca-certificates (no-op); only refresh+install when absent.
                "if [ ! -f /etc/ssl/certs/ca-certificates.crt ]; then\n"
                "  if command -v apt-get >/dev/null 2>&1; then\n"
                "    apt-get update -qq && apt-get install -y -qq ca-certificates"
                " >/dev/null 2>&1 || true;\n"
                "  elif command -v apk >/dev/null 2>&1; then\n"
                "    apk add --no-cache ca-certificates >/dev/null 2>&1 || true;\n"
                "  elif command -v dnf >/dev/null 2>&1; then\n"
                "    dnf install -y -q ca-certificates >/dev/null 2>&1 || true;\n"
                "  fi;\n"
                "fi"
            ),
        )
        # Hand /installed-agent (config, key, state dir) to the agent user so the
        # in-container `aura` can read its key + write sessions.db.
        if environment.default_user:
            await self.exec_as_root(
                environment,
                command=f"chown -R {environment.default_user} {_CONTAINER_DIR}",
            )

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        # One full agent turn to completion. `--timeout 0` = no aura-side limit;
        # Harbor enforces the task's agent timeout. A non-zero aura exit is logged
        # but not raised, so the verifier still grades whatever aura did.
        env = {self._key_env: self._key_value, "AURA_CONFIG_PATH": _CONFIG_PATH}
        cancelled: asyncio.CancelledError | None = None
        try:
            await self.exec_as_agent(
                environment,
                env=env,
                command=(
                    f"aura prompt --json -y --session {_SESSION_ID} --timeout 0 -- "
                    f"{shlex.quote(instruction)}"
                ),
            )
        except asyncio.CancelledError as exc:
            # Harbor enforces the agent timeout by cancelling this coroutine mid-prompt.
            # CancelledError is a BaseException, so `except Exception` won't catch it —
            # capture it, still export the partial trace below, then re-raise so Harbor
            # converts it back to AgentTimeoutError instead of seeing a clean return.
            cancelled = exc
        except Exception as exc:
            self.logger.warning(f"aura prompt exited non-zero (still grading): {exc}")

        await self._export_trace(environment, env)

        if cancelled is not None:
            raise cancelled

    async def _export_trace(
        self, environment: BaseEnvironment, env: dict[str, str]
    ) -> None:
        # Export the transcript + call-tree trace into /logs/agent (mounted to the
        # host trial dir). Best-effort — never fail the task on a trace hiccup. The
        # whole export shares one timeout so a wedged container can't stall cleanup.
        agent_dir = EnvironmentPaths.agent_dir.as_posix()
        trace_env = {**env, "RUST_LOG": "off"}

        async def export() -> None:
            for sub_cmd, fname in (
                (f"session export {_SESSION_ID} --json", "trace.json"),
                (
                    f"session history {_SESSION_ID} --include-superseded --json",
                    "messages.json",
                ),
            ):
                await self.exec_as_agent(
                    environment,
                    env=trace_env,
                    command=(
                        f"mkdir -p {shlex.quote(agent_dir)} && "
                        f"aura {sub_cmd} > {shlex.quote(agent_dir + '/' + fname)} "
                        "2>/dev/null || true"
                    ),
                )

        try:
            await asyncio.wait_for(export(), timeout=_EXPORT_TIMEOUT_SECS)
        except (Exception, asyncio.TimeoutError) as exc:
            self.logger.warning(f"trace export failed (continuing): {exc}")

    def populate_context_post_run(self, context: AgentContext) -> None:
        # Grading is filesystem-based; token/cost telemetry is best-effort and
        # left unset — aura's usage lives in the exported trace under /logs/agent.
        pass
