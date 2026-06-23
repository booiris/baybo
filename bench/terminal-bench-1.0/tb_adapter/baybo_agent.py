"""Terminal-Bench *installed agent* adapter for Baybo.

Runs the real `baybo` binary INSIDE each task's Docker container (the official,
leaderboard-comparable way), driven over tmux by the harness and graded by the
task's own pytest. Mirrors the upstream Codex adapter
(`terminal_bench/agents/installed_agents/codex/`).

Two things make baybo work in-container:
  * a one-shot `baybo prompt --json -y` runs the full agent loop to completion;
  * the container *is* the sandbox, so baybo is configured with
    `sandbox.mode = none` (its Bash runs commands directly — no bwrap, no
    work-dir jail). `none` is a regular config mode — no special build needed —
    the analog of Codex's `--sandbox danger-full-access`.

Use it via the harness's custom-agent flag (no fork of terminal-bench). The env
is uv-managed (see bench/terminal-bench-1.0/pyproject.toml); run from bench/terminal-bench-1.0/:

    uv run --env-file .env tb run \\
        --agent-import-path tb_adapter.baybo_agent:BayboAgent \\
        --model deepseek/deepseek-chat \\
        -d terminal-bench-core==0.1.1 --n-tasks 3
"""

import json
import os
import shlex
import sys
import tempfile
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult
from terminal_bench.agents.installed_agents.abstract_installed_agent import (
    AbstractInstalledAgent,
)
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.terminal.tmux_session import TmuxSession

# Where the binary + config + key live inside the container (the base class
# already uses `/installed-agent` for the install script).
_CONTAINER_DIR = "/installed-agent"
_CONFIG_PATH = f"{_CONTAINER_DIR}/baybo.json"
_BIN_PATH = f"{_CONTAINER_DIR}/baybo"
_BAYBO_HOME = f"{_CONTAINER_DIR}/baybo-home"
_KEY_PATH = f"{_CONTAINER_DIR}/enc.key"
# Fixed session id for the single prompt per task, so the adapter can export that
# session's transcript + trace afterwards (each task is its own container).
_SESSION_ID = "baybo-tb"

# The static-musl baybo binary copied into each container. Defaults to the
# musl build output (repo-root-relative: this file is bench/terminal-bench-1.0/tb_adapter/);
# BAYBO_BIN overrides it (e.g. a glibc build or a custom path).
_DEFAULT_BINARY = (
    Path(__file__).resolve().parents[3]
    / "target"
    / "x86_64-unknown-linux-musl"
    / "release"
    / "baybo"
)

# `--model <provider>/<model>` → the env var baybo reads the provider key from.
# baybo.json references the key *by name*; the value is injected via `_env`.
_PROVIDER_KEY_ENV = {
    "deepseek": "DEEPSEEK_API_KEY",
    "openai": "OPENAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "gemini": "GEMINI_API_KEY",
    "google": "GEMINI_API_KEY",
    "openrouter": "OPENROUTER_API_KEY",
}


class BayboAgent(AbstractInstalledAgent):
    @staticmethod
    def name() -> str:
        return "baybo"

    def __init__(self, model_name: str | None = None, *args, **kwargs):
        super().__init__(*args, **kwargs)
        # Model: `--model <provider>/<model>` (harness flag) or BAYBO_MODEL.
        model_name = model_name or os.environ.get("BAYBO_MODEL")
        if not model_name:
            raise RuntimeError(
                "no model selected: pass `--model <provider>/<model>` or set BAYBO_MODEL"
            )
        provider, sep, model = model_name.partition("/")
        if not sep:
            # No "provider/" prefix — assume the default provider.
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
        # API key value: BAYBO_API_KEY (provider-agnostic) wins, else the
        # provider's own env var. baybo.json references it by name (`api_key_env`)
        # and `_env` injects this value under that name — never written to disk.
        self._key_value = os.environ.get("BAYBO_API_KEY") or os.environ.get(key_env)
        if not self._key_value:
            raise RuntimeError(
                f"set BAYBO_API_KEY or ${key_env} to run the baybo agent for "
                f"provider '{provider}'"
            )

        # Optional custom endpoint (proxy / self-hosted / gateway). Empty = the
        # provider's built-in base URL.
        self._base_url = os.environ.get("BAYBO_BASE_URL") or None

        # Defaults to the musl build output; BAYBO_BIN overrides.
        self._baybo_bin = Path(os.environ.get("BAYBO_BIN") or _DEFAULT_BINARY)
        if not self._baybo_bin.is_file():
            raise RuntimeError(
                f"baybo binary not found at {self._baybo_bin}. Build it with "
                "`cargo build --release --target x86_64-unknown-linux-musl --features bench-bash`, "
                "or set BAYBO_BIN to your binary."
            )

    @property
    def _env(self) -> dict[str, str]:
        # The provider key, sourced into the tmux session before install so it
        # persists for the `baybo prompt` run. baybo.json names this env var
        # (`api_key_env`); the value (from BAYBO_API_KEY or the provider's own
        # env var) is injected here under that name and never written to disk.
        return {self._key_env: self._key_value}

    @property
    def _install_agent_script_path(self) -> Path:
        return self._get_templated_script_path("baybo-setup.sh.j2")

    def _baybo_config(self) -> dict:
        """The baybo.json rendered into the container — `none` sandbox, a
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
            "workspace": {"path": _BAYBO_HOME},
            "security": {
                "encryption_key_file": _KEY_PATH,
                "leak_detection_enabled": False,
            },
            # Required to pass config validation even though `baybo prompt` runs
            # in-process and never binds.
            "gateway": {"bind_address": "127.0.0.1", "port": 8723},
            "sandbox": {"mode": "none"},
            "cost": {"rate_limit": {"max_requests": 1_000_000}},
        }

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        # The base class copies only the install script. baybo needs its binary
        # and a pre-rendered config in the container too — copy both first, then
        # run the standard install + env-setup + run flow.
        session.copy_to_container(
            self._baybo_bin, container_dir=_CONTAINER_DIR, container_filename="baybo"
        )
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False
        ) as cfg:
            json.dump(self._baybo_config(), cfg)
            cfg_path = Path(cfg.name)
        session.copy_to_container(
            cfg_path, container_dir=_CONTAINER_DIR, container_filename="baybo.json"
        )
        result = super().perform_task(instruction, session, logging_dir)
        if not os.environ.get("NO_TRACE"):
            self._export_trace(session, logging_dir)
        return result

    def _export_trace(
        self, session: TmuxSession, logging_dir: Path | None
    ) -> None:
        """Dump baybo's verbatim transcript + call-tree trace out of the container
        into the bench `trace/` dir, mirroring the harness's per-task path under
        `runs/`. Uses the container handle directly (`exec_run`) — each command
        prints JSON to stdout, so no copy-out is needed. Best-effort: a failure is
        reported but swallowed so a trace hiccup never fails the task. NO_TRACE
        disables it (checked by the caller)."""
        trace_base = Path(
            os.environ.get("BAYBO_TRACE_DIR")
            or (Path(__file__).resolve().parents[1] / "trace")
        )
        if logging_dir is not None:
            parts = logging_dir.resolve().parts
            sub = parts[parts.index("runs") + 1 :] if "runs" in parts else parts[-3:]
            out_dir = trace_base.joinpath(*sub) if sub else trace_base / _SESSION_ID
        else:
            out_dir = trace_base / _SESSION_ID
        out_dir.mkdir(parents=True, exist_ok=True)
        dumps = [
            (
                ["session", "history", _SESSION_ID, "--include-superseded", "--json"],
                "messages.json",
            ),
            (["session", "export", _SESSION_ID, "--json"], "trace.json"),
        ]
        for sub_cmd, fname in dumps:
            try:
                code, output = session.container.exec_run(
                    ["baybo", *sub_cmd],
                    environment={"BAYBO_CONFIG_PATH": _CONFIG_PATH, "RUST_LOG": "off"},
                )
                if code == 0:
                    (out_dir / fname).write_bytes(output)
            except Exception as e:  # best-effort: never fail the task on a trace problem
                print(f"baybo trace export ({fname}) failed: {e}", file=sys.stderr)

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        # One full agent turn to completion. `--timeout 0` = no baybo-side limit;
        # the harness enforces the task's `max_agent_timeout_sec`.
        return [
            TerminalCommand(
                command=(
                    f"BAYBO_CONFIG_PATH={_CONFIG_PATH} "
                    f"baybo prompt --json -y --session {_SESSION_ID} --timeout 0 -- "
                    f"{shlex.quote(instruction)}"
                ),
                min_timeout_sec=0.0,
                max_timeout_sec=float("inf"),
                block=True,
                append_enter=True,
            )
        ]
