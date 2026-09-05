#!/usr/bin/env bash
# Ship the current (persistent) dev branch to its base and re-sync it.
#
# Flow: push -> open/reuse PR -> wait for CI -> merge (merge commit) -> fast-forward
# the dev branch onto the merge commit -> push. The dev branch is NEVER deleted
# (it is long-lived); after the merge it equals the base again, so work continues
# on it. Merge strategy is a merge commit (matches this repo's PR convention) so
# the persistent branch fast-forwards cleanly with no orphaned/duplicated commits.
#
# Usage: scripts/dev-merge-sync.sh [-y] [-b base] [-t "PR title"]
#   -y         skip the pre-merge confirmation
#   -b base    base branch to merge into (default: master)
#   -t title   PR title when creating a new PR (default: latest commit subject)
#
# Requires: authenticated `gh`, a clean working tree (tracked changes), and being
# on the dev branch (not the base). Untracked files are ignored, so this script
# living uncommitted under scripts/ does not block itself.
set -euo pipefail

# The GATING jobs of .github/workflows/ci.yml, by check name. Every one must
# report a real `pass` before we merge.
#
# Waiting on the checks does NOT establish that, which is the whole reason this
# list exists: every job in ci.yml carries `if: draft == false`, so on a draft
# PR they all skip — and `gh pr checks --watch --fail-fast` reports each one as
# `skipping` and exits 0, which is indistinguishable from green. This script
# would then merge code CI has never compiled. GitHub's own required-status
# checks would not save us either: a skipped check counts as SATISFIED, which is
# the same hole. (The repo is public now, so rulesets ARE available and one is
# active on master — "Protect master": no deletion, no force-push, PR required
# with 1 approval. It carries no required-status-checks rule, so it does not
# check CI either; this list is still the only thing that does. The owner IS a
# bypass actor with `bypass_mode: always` — but `gh pr merge` does not exercise
# that bypass unless it is handed `--admin`, and GitHub forbids approving your
# own PR, so the merge below stops on "the base branch policy prohibits the
# merge" and finishing a solo ship is a deliberate manual step. Left that way on
# purpose: stepping over branch protection should be a decision made each time,
# not a flag this script carries.)
#
# Deliberately not required: "detect changes" (a path-filter helper for the
# other jobs) and "tmux render tests (non-gating)" (named non-gating; flaky
# under load, and conditional).
#
# The three iOS jobs and the install.sh job ARE required, but only when this PR
# should have run them — see the path filters below.
REQUIRED_CHECKS=(
  "rustfmt"
  "clippy"
  "cargo test"
  "ts-rs bindings sync"
  "frontend (typecheck + build + test)"
)

# The iOS jobs are path-filtered by ci.yml's "detect changes", so a skip is
# legitimate on a PR that touches nothing iOS — which is why they cannot simply
# join the list above. The failure mode this guards is the other one: PR #206
# merged with the five checks above green and BOTH iOS jobs skipped, on a PR
# that was ~half app/ios (the final catch-up merge's incremental diff was
# non-iOS, so the filter said false). A skip is only acceptable here if this
# PR's own diff agrees it should be one.
#
# The two patterns MUST mirror `IOS_DEPS` and the ios_native filter in
# .github/workflows/ci.yml. They differ on purpose: a docs-only change under
# app/ios runs the two Linux jobs but never queues for the Mac one.
IOS_DEPS_PATTERN='^(app/ios/|crates/(wire|device-proto|model)/|remote-host/|docs/openapi\.json)'
IOS_NATIVE_PATTERN='^app/ios/(App/|UITests/|Tests/|NotificationExtension/|project\.yml|scripts/|web/|ffi/|bindgen/|Cargo\.)|^crates/(wire|device-proto|model)/|^remote-host/'

# Same shape, same reason, for the install/release shell stack — and the sharper
# edge of the two: `install.sh` is the only user-facing file no other gate can
# run (before a release exists it has nothing to install, and release.yml is
# dispatch-only), so its container matrix is the only thing between a broken
# one-liner and every new user. MUST mirror the `install` filter in
# .github/workflows/ci.yml. `deploy/docker/Dockerfile` belongs to it because
# release-build.sh reads the node/bun/pnpm pins out of that file.
INSTALL_PATTERN='^(install\.sh|scripts/(release-build|test-install)\.sh|\.github/workflows/release\.yml|deploy/docker/Dockerfile)$'

BASE="master"
ASSUME_YES=0
TITLE=""
while getopts "yb:t:" opt; do
  case "$opt" in
    y) ASSUME_YES=1 ;;
    b) BASE="$OPTARG" ;;
    t) TITLE="$OPTARG" ;;
    *) echo "usage: $0 [-y] [-b base] [-t title]" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }

command -v gh >/dev/null || die "gh CLI not found"
DEV="$(git rev-parse --abbrev-ref HEAD)"
[ "$DEV" != "$BASE" ] || die "on '$BASE' — run from your dev branch, not the base"
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "uncommitted changes — commit or stash first"

echo "==> dev branch: $DEV  ->  base: $BASE"
git fetch origin --quiet

AHEAD="$(git rev-list --count "origin/$BASE..$DEV")"
[ "$AHEAD" -gt 0 ] || die "'$DEV' has no commits over origin/$BASE — nothing to ship"
[ "$(git rev-list --count "$DEV..origin/$BASE")" -eq 0 ] || die "'$DEV' is behind origin/$BASE — rebase/merge it first"
echo "==> $AHEAD commit(s) to ship"

CHANGED="$(git diff --name-only "origin/$BASE...$DEV")"
if printf '%s\n' "$CHANGED" | grep -qE "$IOS_DEPS_PATTERN"; then
  REQUIRED_CHECKS+=("iOS transcript bundle (lint + test + build)" "iOS core (rust)")
  echo "==> diff touches iOS deps: requiring the two Linux iOS checks"
fi
if printf '%s\n' "$CHANGED" | grep -qE "$IOS_NATIVE_PATTERN"; then
  REQUIRED_CHECKS+=("iOS app (build + unit tests; UI smokes non-gating)")
  echo "==> diff touches the Swift/ffi half: requiring the macOS iOS check"
fi
if printf '%s\n' "$CHANGED" | grep -qE "$INSTALL_PATTERN"; then
  REQUIRED_CHECKS+=("install.sh (shellcheck + container matrix)")
  echo "==> diff touches the install/release shell stack: requiring the install.sh check"
fi

git push -u origin "$DEV"

# Reuse an open PR for this head->base, else create one.
PR="$(gh pr list --head "$DEV" --base "$BASE" --state open --json number -q '.[0].number' || true)"
if [ -z "$PR" ]; then
  if [ -n "$TITLE" ]; then
    BODY="$(git log "origin/$BASE..$DEV" --format='- %s')"
    gh pr create --base "$BASE" --head "$DEV" --title "$TITLE" --body "$BODY"
  else
    gh pr create --base "$BASE" --head "$DEV" --fill
  fi
  PR="$(gh pr list --head "$DEV" --base "$BASE" --state open --json number -q '.[0].number' || true)"
  echo "==> created PR #$PR"
else
  echo "==> reusing PR #$PR"
fi
[ -n "$PR" ] || die "could not resolve a PR number"

# A draft runs no CI at all, so there would be nothing to verify below. Marking
# it ready is the owner's call, not this script's (/CLAUDE.md) — and it is also
# what fires `ready_for_review` and lets CI through.
[ "$(gh pr view "$PR" --json isDraft -q .isDraft)" = "false" ] ||
  die "PR #$PR is a draft: CI does not run on drafts. The owner marks it ready ('gh pr ready $PR'), then re-run this."

echo "==> waiting for checks on PR #$PR ..."
gh pr checks "$PR" --watch --fail-fast || die "checks failed/cancelled on PR #$PR — not merging"

# Trust the results, not the wait: see REQUIRED_CHECKS above for why exiting 0
# proves nothing on its own.
echo "==> verifying the gating checks reported a real pass ..."
CHECKS="$(gh pr checks "$PR" --json name,bucket --jq '.[] | "\(.bucket)\t\(.name)"')" ||
  die "could not read the checks for PR #$PR"
for name in "${REQUIRED_CHECKS[@]}"; do
  buckets="$(printf '%s\n' "$CHECKS" | awk -F'\t' -v n="$name" '$2 == n { print $1 }')"
  if [ -z "$buckets" ]; then
    die "required check '$name' never reported on PR #$PR — CI has not seen this code (renamed in ci.yml?). Not merging."
  fi
  if printf '%s\n' "$buckets" | grep -qxv 'pass'; then
    die "required check '$name' is '$(printf '%s' "$buckets" | tr '\n' ',')', not a pass. Not merging."
  fi
  echo "    pass  $name"
done

if [ "$ASSUME_YES" -ne 1 ]; then
  printf "==> merge PR #%s into %s and fast-forward %s? [y/N] " "$PR" "$BASE" "$DEV"
  read -r ans
  case "$ans" in y | Y | yes | YES) ;; *) die "aborted" ;; esac
fi

# Merge commit; keep the branch (it is the persistent dev branch).
gh pr merge "$PR" --merge

# Re-sync: ff the dev branch onto the merged base, push (recreates it if the merge
# auto-deleted the remote head), and point the local base ref at the merge commit.
git fetch origin --quiet
git merge --ff-only "origin/$BASE"
git push origin "$DEV"
git branch -f "$BASE" "origin/$BASE"

echo "==> done: $DEV == $BASE == $(git rev-parse --short "origin/$BASE")"
