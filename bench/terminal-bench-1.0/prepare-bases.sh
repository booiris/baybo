#!/usr/bin/env bash
# Pre-bake the Terminal-Bench test-phase toolchain into the base images.
#
# Every task's grader runs tests/setup-uv-pytest.sh, which installs curl + uv +
# pytest AT TEST TIME (apt-get install curl; curl astral.sh/uv | sh; uv add
# pytest). On a slow network that bootstrap re-downloads tens of MB for *every*
# task and overruns tb's 60s max_test_timeout_sec, so a task the agent SOLVED
# still scores `test_timeout`. This bakes those tools — and a warmed pytest
# cache — into the two core base images ONCE, so each task's bootstrap runs from
# local cache (apt curl = already present, `uv add pytest` = offline).
#
# Mechanism: build an augmented image FROM the pristine base and re-tag it as the
# base's own name. The task docker-compose build sets no pull_policy, so task
# image builds layer on whatever is local for that tag. The pristine base is kept
# under a ':pristine' tag, so this is idempotent and reversible:
#     docker tag ghcr.io/laude-institute/t-bench/<base>:pristine \
#               ghcr.io/laude-institute/t-bench/<base>:latest      # revert
#
#   ./prepare-bases.sh           # augment both core bases (skips already-done ones)
#   FORCE=1 ./prepare-bases.sh   # rebuild even if already augmented
set -euo pipefail

registry="ghcr.io/laude-institute/t-bench"
bases=(ubuntu-24-04 python-3-13)
label="baybo.tb.augmented"

for base in "${bases[@]}"; do
  img="$registry/${base}:latest"
  pristine="$registry/${base}:pristine"

  if [[ -z "${FORCE:-}" ]] &&
     [[ "$(docker image inspect --format "{{index .Config.Labels \"$label\"}}" "$img" 2>/dev/null || true)" == "1" ]]; then
    echo "==> $base already augmented — skipping (FORCE=1 to rebuild)" >&2
    continue
  fi

  # Capture the pristine base once, before the first augmentation overwrites
  # :latest. Only pull when :latest isn't already local (ghcr is slow here).
  if ! docker image inspect "$pristine" >/dev/null 2>&1; then
    if ! docker image inspect "$img" >/dev/null 2>&1; then
      echo "==> pulling $base (not local)…" >&2
      docker pull "$img"
    fi
    docker tag "$img" "$pristine"
  fi

  echo "==> augmenting $base (curl + uv + warmed pytest cache); first time is slow…" >&2
  ctx="$(mktemp -d)"
  {
    echo "FROM $pristine"
    cat <<'DOCKERFILE'
# Keep /var/lib/apt/lists populated (do NOT rm) so the grader's own `apt-get
# update` is a fast conditional 304 refresh (~6s) instead of a cold fetch (~30-50s).
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
# Pre-stage the uv release artifact and point the official installer at it
# (file://): the grader's `curl astral.sh/uv | sh` then unpacks uv from local disk
# (~3s) instead of re-downloading ~25MB (~26s) — the installer is not idempotent
# but honours INSTALLER_DOWNLOAD_URL (a base dir; it appends the platform artifact
# name). Pinned to the version this image installed; re-run prepare-bases.sh
# (FORCE=1) if uv ships a new release before a fresh bench.
RUN . $HOME/.local/bin/env \
 && uvver="$(uv --version | awk '{print $2}')" \
 && mkdir -p /opt/uvdl \
 && curl -fsSL "https://github.com/astral-sh/uv/releases/download/${uvver}/uv-x86_64-unknown-linux-gnu.tar.gz" \
      -o /opt/uvdl/uv-x86_64-unknown-linux-gnu.tar.gz
ENV INSTALLER_DOWNLOAD_URL=file:///opt/uvdl
# Warm uv's global cache with pytest so the grader's `uv add pytest` is offline-fast.
RUN . $HOME/.local/bin/env \
 && uv venv /tmp/_warm \
 && uv pip install --python /tmp/_warm pytest \
 && rm -rf /tmp/_warm
DOCKERFILE
    echo "LABEL $label=1"
  } >"$ctx/Dockerfile"
  docker build -t "$img" "$ctx"
  rm -rf "$ctx"
done

echo "==> done; base images:" >&2
docker images "$registry/*" --format '  {{.Repository}}:{{.Tag}}  {{.Size}}' 2>/dev/null \
  | grep -E ':(latest|pristine)' >&2 || true
